//! Dependency resolution with semver constraints.
//!
//! Algorithm:
//! 1. Resolve package refs/versions (MVS for git sources)
//! 2. Resolve package manifests bottom-up (deps before item seeds)
//! 3. Traverse items with DFS from seeded requests and frontmatter skill deps
//! 4. Emit deterministic alphabetical package order
//!
//! Uses `semver` crate for all version parsing. No custom version logic.

pub mod compat;
mod constraint;
mod context;
mod filter;
mod package;
mod path;
mod skill;
mod types;
mod version;

use std::path::Path;

#[cfg(test)]
use indexmap::IndexMap;

pub use constraint::parse_version_constraint;
pub use context::ResolverContext;
pub use types::*;

pub(crate) use package::{PackageResolutionState, PendingSource, RegisteredPackage};
#[cfg(test)]
pub(crate) use path::apply_subpath;

use crate::config::{EffectiveConfig, Manifest, SourceSpec};
use crate::diagnostic::DiagnosticCollector;
use crate::error::{MarsError, ResolutionError};
use crate::lock::LockFile;
use crate::source::{AvailableVersion, ResolvedRef};
#[cfg(test)]
use crate::types::SourceName;
use crate::types::SourceUrl;
use filter::is_item_excluded;
use package::resolve_package_bottom_up;
use skill::{parse_pending_item_skill_deps, resolve_skill_ref};
use version::validate_all_constraints;

#[derive(Debug)]
enum VersionAction {
    Process,
    Skip,
}

fn apply_item_version_policy(
    pending_item: &PendingItem,
    check: VersionCheckResult,
    diag: &mut DiagnosticCollector,
) -> Result<VersionAction, ResolutionError> {
    match check {
        VersionCheckResult::NotSeen => Ok(VersionAction::Process),
        VersionCheckResult::SameVersion => Ok(VersionAction::Skip),
        VersionCheckResult::PotentiallyConflicting {
            existing,
            requested,
        } => {
            diag.warn(
                "potential-version-drift",
                format!(
                    "potential version drift: item '{}' from '{}' requested as {} but already seen as {}",
                    pending_item.item, pending_item.package, requested, existing
                ),
            );
            Ok(VersionAction::Skip)
        }
        VersionCheckResult::DifferentVersion {
            existing,
            requested,
        } => {
            if pending_item.is_local {
                return Ok(VersionAction::Skip);
            }
            Err(ResolutionError::ItemVersionConflict {
                item: pending_item.item.to_string(),
                package: pending_item.package.to_string(),
                existing: existing.to_string(),
                requested: requested.to_string(),
                chain: pending_item.required_by.clone(),
            })
        }
    }
}

/// Lists semver-tagged versions available for a git source.
pub trait VersionLister {
    fn list_versions(&self, url: &SourceUrl) -> Result<Vec<AvailableVersion>, MarsError>;
}

/// Fetches concrete source trees after the resolver has picked a strategy.
pub trait SourceFetcher {
    /// Fetch a git source at a specific version tag.
    fn fetch_git_version(
        &self,
        url: &SourceUrl,
        version: &AvailableVersion,
        source_name: &str,
        preferred_commit: Option<&str>,
        diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError>;

    /// Fetch a git source at a branch/commit ref (non-semver path).
    fn fetch_git_ref(
        &self,
        url: &SourceUrl,
        ref_name: &str,
        source_name: &str,
        preferred_commit: Option<&str>,
        diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError>;

    /// Resolve a local path source into a concrete tree reference.
    fn fetch_path(
        &self,
        path: &Path,
        source_name: &str,
        diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError>;
}

/// Reads source manifests for transitive dependency discovery.
pub trait ManifestReader {
    fn read_manifest(
        &self,
        source_tree: &Path,
        diag: &mut DiagnosticCollector,
    ) -> Result<Option<Manifest>, MarsError>;
}

/// Composite trait used by `resolve()`.
pub trait SourceProvider: VersionLister + SourceFetcher + ManifestReader {}

impl<T> SourceProvider for T where T: VersionLister + SourceFetcher + ManifestReader {}

/// Resolve the full dependency graph from config.
///
/// Uses Minimum Version Selection (MVS) by default: selects the lowest
/// version satisfying all constraints. This is conservative and reproducible —
/// the same constraint always resolves to the same version. Users who want
/// the latest use `@latest` explicitly, or `mars upgrade`.
///
/// When `locked` is provided, prefer locked versions when constraints allow
/// (reproducible builds).
pub fn resolve(
    config: &EffectiveConfig,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    diag: &mut DiagnosticCollector,
) -> Result<ResolvedGraph, MarsError> {
    let mut ctx = ResolverContext::new();

    let mut direct_requests: Vec<PendingSource> = Vec::new();
    for (name, source) in &config.dependencies {
        let is_upgrade_target = options.maximize
            && (options.upgrade_targets.is_empty() || options.upgrade_targets.contains(name));
        let constraint = match &source.spec {
            SourceSpec::Git(git) => {
                if options.bump_direct_constraints && is_upgrade_target {
                    VersionConstraint::Latest
                } else {
                    parse_version_constraint(git.version.as_deref())
                }
            }
            SourceSpec::Path(_) => VersionConstraint::Latest,
        };
        direct_requests.push(PendingSource {
            name: name.clone(),
            source_id: source.id.clone(),
            spec: source.spec.clone(),
            subpath: source.subpath.clone(),
            constraint,
            filter: source.filter.clone(),
            required_by: "mars.toml".to_string(),
        });
    }

    for request in direct_requests
        .iter()
        .filter(|request| filter::is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(request, true, provider, locked, options, diag, &mut ctx)?;
    }
    for request in direct_requests
        .iter()
        .filter(|request| !filter::is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(request, true, provider, locked, options, diag, &mut ctx)?;
    }

    while let Some(pending_item) = ctx.pop_pending() {
        let (resolved_ref, skill_deps) = {
            let Some(package) = ctx.registry().get(&pending_item.package) else {
                return Err(ResolutionError::SourceNotFound {
                    name: pending_item.package.to_string(),
                }
                .into());
            };

            if package
                .item(pending_item.kind, &pending_item.item)
                .is_none()
            {
                continue;
            }

            let skill_deps = parse_pending_item_skill_deps(&pending_item, package)?;
            (package.node.resolved_ref.clone(), skill_deps)
        };

        match apply_item_version_policy(
            &pending_item,
            ctx.visited().check_version(
                &pending_item.package,
                &pending_item.item,
                &pending_item.constraint,
            ),
            diag,
        )
        .map_err(MarsError::from)?
        {
            VersionAction::Process => {}
            VersionAction::Skip => continue,
        }

        ctx.package_versions_mut()
            .check_or_insert(
                &pending_item.package,
                &resolved_ref,
                &pending_item.constraint,
                &pending_item.required_by,
                pending_item.is_local,
            )
            .map_err(MarsError::from)?;

        ctx.visited_mut().insert(
            pending_item.package.clone(),
            pending_item.item.clone(),
            pending_item.constraint.clone(),
            resolved_ref,
        );

        for skill_dep in skill_deps {
            let resolved_skill = resolve_skill_ref(
                &skill_dep,
                &pending_item,
                ctx.registry(),
                ctx.version_constraints(),
            )?;
            if is_item_excluded(
                ctx.materialization_filters(),
                ctx.registry(),
                &resolved_skill.package,
                resolved_skill.kind,
                &resolved_skill.item,
            ) {
                continue;
            }
            ctx.add_filter(
                &resolved_skill.package,
                crate::config::FilterMode::Include {
                    agents: Vec::new(),
                    skills: vec![resolved_skill.item.clone()],
                },
            );
            ctx.push_pending(resolved_skill);
        }
    }

    let version_constraints = ctx.version_constraints().clone();
    let graph = ctx.into_graph();

    validate_all_constraints(&graph.nodes, &version_constraints)?;

    Ok(graph)
}

#[cfg(test)]
fn alphabetical_order(nodes: &IndexMap<SourceName, ResolvedNode>) -> Vec<SourceName> {
    let mut order: Vec<SourceName> = nodes.keys().cloned().collect();
    order.sort();
    order
}

#[cfg(test)]
mod tests;
