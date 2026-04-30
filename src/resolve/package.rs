use std::collections::HashSet;
use std::path::Path;

use crate::config::{FilterMode, GitSpec, Manifest, SourceSpec};
use crate::diagnostic::DiagnosticCollector;
use crate::discover;
use crate::error::{ConfigError, MarsError, ResolutionError};
use crate::lock::{ItemKind, LockFile};
use crate::types::{ItemName, SourceId, SourceName, SourceSubpath};
use indexmap::IndexMap;

use super::SourceProvider;
use super::constraint::parse_version_constraint;
use super::context::ResolverContext;
use super::filter::is_unfiltered_request;
use super::path::{apply_subpath, source_id_for_pending_spec};
use super::types::{PendingItem, ResolveOptions, ResolvedNode, VersionConstraint};
use super::version::resolve_single_source;

/// Internal: a source waiting to be resolved.
#[derive(Debug, Clone)]
pub(crate) struct PendingSource {
    pub(crate) name: SourceName,
    pub(crate) source_id: SourceId,
    pub(crate) spec: SourceSpec,
    pub(crate) subpath: Option<SourceSubpath>,
    pub(crate) constraint: VersionConstraint,
    pub(crate) filter: FilterMode,
    pub(crate) required_by: String,
}

#[derive(Debug, Default)]
pub(crate) enum PackageResolutionState {
    #[default]
    Resolved,
    Resolving {
        deferred_seed_requests: Vec<PendingSource>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredPackage {
    pub(crate) node: ResolvedNode,
    pub(crate) items: IndexMap<(ItemKind, ItemName), discover::DiscoveredItem>,
    pub(crate) constraint: VersionConstraint,
    pub(crate) spec: SourceSpec,
    pub(crate) is_local: bool,
}

impl RegisteredPackage {
    pub(crate) fn items(&self) -> impl Iterator<Item = &discover::DiscoveredItem> {
        self.items.values()
    }

    pub(crate) fn item(
        &self,
        kind: ItemKind,
        name: &ItemName,
    ) -> Option<&discover::DiscoveredItem> {
        self.items.get(&(kind, name.clone()))
    }

    pub(crate) fn has_skill(&self, skill: &ItemName) -> bool {
        self.skill_names().any(|name| name == skill)
    }

    pub(crate) fn skill_names(&self) -> impl Iterator<Item = &ItemName> {
        self.items
            .keys()
            .filter(|(kind, _)| *kind == ItemKind::Skill)
            .map(|(_, name)| name)
    }
}

pub(crate) fn resolve_package_bottom_up(
    pending_src: &PendingSource,
    seed_items: bool,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    diag: &mut DiagnosticCollector,
    ctx: &mut ResolverContext,
) -> Result<(), MarsError> {
    if let Some(existing_name) = ctx.id_index().get(&pending_src.source_id)
        && existing_name != &pending_src.name
    {
        return Err(ResolutionError::DuplicateSourceIdentity {
            existing_name: existing_name.to_string(),
            duplicate_name: pending_src.name.to_string(),
            source_id: pending_src.source_id.to_string(),
        }
        .into());
    }

    if let Some(existing_package) = ctx.registry().get(&pending_src.name)
        && existing_package.node.source_id != pending_src.source_id
    {
        return Err(ResolutionError::SourceIdentityMismatch {
            name: pending_src.name.to_string(),
            existing: existing_package.node.source_id.to_string(),
            incoming: pending_src.source_id.to_string(),
        }
        .into());
    }

    ctx.add_version_constraint(
        &pending_src.name,
        &pending_src.required_by,
        pending_src.constraint.clone(),
    );
    if seed_items {
        ctx.add_filter(&pending_src.name, pending_src.filter.clone());
    }

    if matches!(
        ctx.package_states().get(&pending_src.name),
        Some(PackageResolutionState::Resolved)
    ) {
        if seed_items {
            let package =
                ctx.registry()
                    .get(&pending_src.name)
                    .ok_or_else(|| MarsError::Source {
                        source_name: pending_src.name.to_string(),
                        message: "resolved package missing from registry".to_string(),
                    })?;
            for pending_item in seed_items_for_request(pending_src, package) {
                ctx.push_pending(pending_item);
            }
        }
        return Ok(());
    }

    if matches!(
        ctx.package_states().get(&pending_src.name),
        Some(PackageResolutionState::Resolving { .. })
    ) {
        if seed_items
            && let Some(PackageResolutionState::Resolving {
                deferred_seed_requests,
            }) = ctx.package_states_mut().get_mut(&pending_src.name)
        {
            deferred_seed_requests.push(pending_src.clone());
        }
        return Ok(());
    }

    ctx.package_states_mut().insert(
        pending_src.name.clone(),
        PackageResolutionState::Resolving {
            deferred_seed_requests: Vec::new(),
        },
    );

    let (resolved_ref, latest_version) = resolve_single_source(
        pending_src,
        provider,
        locked,
        options,
        ctx.version_constraints(),
        diag,
    )?;
    let rooted_ref = apply_subpath(
        &pending_src.name,
        &resolved_ref.tree_path,
        pending_src.subpath.as_ref(),
    )?;
    let manifest = provider.read_manifest(&rooted_ref.package_root, diag)?;
    let manifest_requests =
        collect_manifest_requests(pending_src, &rooted_ref.package_root, &manifest)?;
    let deps = manifest_requests
        .iter()
        .map(|request| request.name.clone())
        .collect();

    let discovered = discover::discover_resolved_source(
        &rooted_ref.package_root,
        Some(pending_src.name.as_ref()),
    )?;
    let mut items: IndexMap<(ItemKind, ItemName), discover::DiscoveredItem> = IndexMap::new();
    for item in &discovered {
        items.insert((item.id.kind, item.id.name.clone()), item.clone());
    }

    ctx.registry_mut().insert(
        pending_src.name.clone(),
        RegisteredPackage {
            node: ResolvedNode {
                source_name: pending_src.name.clone(),
                source_id: pending_src.source_id.clone(),
                rooted_ref,
                resolved_ref,
                latest_version,
                manifest,
                deps,
            },
            items,
            constraint: pending_src.constraint.clone(),
            spec: pending_src.spec.clone(),
            is_local: matches!(pending_src.spec, SourceSpec::Path(_)),
        },
    );
    ctx.id_index_mut()
        .insert(pending_src.source_id.clone(), pending_src.name.clone());

    // Version graph expansion is always required, but transitive item seeding is
    // only allowed when this package has at least one unfiltered materialization
    // request and the inbound path has remained unfiltered.
    let seed_transitive_manifest_deps =
        seed_items && package_has_unfiltered_materialization_request(ctx, &pending_src.name);
    for request in manifest_requests
        .iter()
        .filter(|request| is_unfiltered_request(&request.filter))
    {
        let seed_request_items = seed_transitive_manifest_deps;
        resolve_package_bottom_up(
            request,
            seed_request_items,
            provider,
            locked,
            options,
            diag,
            ctx,
        )?;
    }
    for request in manifest_requests
        .iter()
        .filter(|request| !is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(request, false, provider, locked, options, diag, ctx)?;
    }

    let mut deferred_seed_requests = Vec::new();
    if let Some(PackageResolutionState::Resolving {
        deferred_seed_requests: deferred,
    }) = ctx.package_states_mut().remove(&pending_src.name)
    {
        deferred_seed_requests = deferred;
    }
    ctx.package_states_mut()
        .insert(pending_src.name.clone(), PackageResolutionState::Resolved);

    let pending_to_push = {
        let package = ctx
            .registry()
            .get(&pending_src.name)
            .ok_or_else(|| MarsError::Source {
                source_name: pending_src.name.to_string(),
                message: "resolved package missing from registry".to_string(),
            })?;
        let mut pending_to_push = Vec::new();
        if seed_items {
            pending_to_push.extend(seed_items_for_request(pending_src, package));
        }
        for deferred_request in deferred_seed_requests {
            pending_to_push.extend(seed_items_for_request(&deferred_request, package));
        }
        pending_to_push
    };
    for pending_item in pending_to_push {
        ctx.push_pending(pending_item);
    }

    Ok(())
}

fn package_has_unfiltered_materialization_request(
    ctx: &ResolverContext,
    package: &SourceName,
) -> bool {
    ctx.materialization_filters()
        .get(package)
        .is_some_and(|filters| filters.iter().any(is_unfiltered_request))
}

pub(crate) fn seed_items_for_request(
    pending_src: &PendingSource,
    package: &RegisteredPackage,
) -> Vec<PendingItem> {
    let mut selected: Vec<&discover::DiscoveredItem> = Vec::new();
    match &pending_src.filter {
        FilterMode::All => {
            selected.extend(package.items());
        }
        FilterMode::Include { agents, skills } => {
            let wanted_agents: HashSet<ItemName> = agents.iter().cloned().collect();
            let wanted_skills: HashSet<ItemName> = skills.iter().cloned().collect();
            selected.extend(package.items().filter(|item| match item.id.kind {
                ItemKind::Agent => wanted_agents.contains(&item.id.name),
                ItemKind::Skill => wanted_skills.contains(&item.id.name),
                // New kinds not yet selectable via Include filter.
                ItemKind::Hook | ItemKind::McpServer | ItemKind::BootstrapDoc => false,
            }));
        }
        FilterMode::Exclude(excluded) => {
            selected.extend(package.items().filter(|item| {
                let source_path = item.source_path.to_string_lossy();
                !excluded.iter().any(|excluded_item| {
                    excluded_item == &item.id.name || excluded_item == source_path.as_ref()
                })
            }));
        }
        FilterMode::OnlySkills => {
            selected.extend(
                package
                    .items()
                    .filter(|item| item.id.kind == ItemKind::Skill),
            );
        }
        FilterMode::OnlyAgents => {
            selected.extend(
                package
                    .items()
                    .filter(|item| item.id.kind == ItemKind::Agent),
            );
        }
    }

    selected
        .into_iter()
        .map(|item| PendingItem {
            package: pending_src.name.clone(),
            item: item.id.name.clone(),
            kind: item.id.kind,
            constraint: pending_src.constraint.clone(),
            required_by: pending_src.required_by.clone(),
            is_local: package.is_local,
            spec: pending_src.spec.clone(),
        })
        .collect()
}

pub(crate) fn collect_manifest_requests(
    pending_src: &PendingSource,
    package_root: &Path,
    manifest: &Option<Manifest>,
) -> Result<Vec<PendingSource>, MarsError> {
    let mut requests = Vec::new();
    let Some(manifest_data) = manifest else {
        return Ok(requests);
    };
    for (dep_name, dep_spec) in &manifest_data.dependencies {
        let dep_name_typed = SourceName::from(dep_name.clone());
        let dep_subpath = dep_spec.subpath.clone();
        let dep_filter = dep_spec.filter.to_mode();

        let (dep_spec_resolved, dep_constraint) = match (&dep_spec.url, &dep_spec.path) {
            (Some(url), None) => (
                SourceSpec::Git(GitSpec {
                    url: url.clone(),
                    version: dep_spec.version.clone(),
                }),
                parse_version_constraint(dep_spec.version.as_deref()),
            ),
            (None, Some(path)) => {
                let resolved_path = if path.is_absolute() {
                    path.clone()
                } else {
                    package_root.join(path)
                };
                (SourceSpec::Path(resolved_path), VersionConstraint::Latest)
            }
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid {
                    message: format!("source `{dep_name}` has both `url` and `path` — pick one"),
                }
                .into());
            }
            (None, None) => {
                return Err(ConfigError::Invalid {
                    message: format!(
                        "source `{dep_name}` has neither `url` nor `path` — one is required"
                    ),
                }
                .into());
            }
        };

        let dep_source_id =
            source_id_for_pending_spec(package_root, &dep_spec_resolved, dep_subpath.clone());
        requests.push(PendingSource {
            name: dep_name_typed,
            source_id: dep_source_id,
            spec: dep_spec_resolved,
            subpath: dep_subpath,
            constraint: dep_constraint,
            filter: dep_filter,
            required_by: pending_src.name.to_string(),
        });
    }

    Ok(requests)
}
