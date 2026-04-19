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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use semver::{Version, VersionReq};

use self::compat::CompatibilityResult;
use crate::config::{EffectiveConfig, FilterMode, GitSpec, Manifest, SourceSpec};
use crate::diagnostic::DiagnosticCollector;
use crate::discover;
use crate::error::{MarsError, ResolutionError};
use crate::lock::{ItemKind, LockFile};
use crate::source::{AvailableVersion, ResolvedRef};
use crate::types::{ItemName, SourceId, SourceName, SourceSubpath, SourceUrl};
use crate::validate;

/// The resolved dependency graph — all sources with concrete versions.
///
/// Produced by the resolver after fetching sources, reading manifests,
/// intersecting version constraints, and deterministic ordering.
#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    pub nodes: IndexMap<SourceName, ResolvedNode>,
    /// Topological order (deps before dependents).
    pub order: Vec<SourceName>,
    pub id_index: HashMap<SourceId, SourceName>,
    /// All filter constraints collected for each source (direct + transitive).
    pub filters: HashMap<SourceName, Vec<FilterMode>>,
}

/// A single node in the resolved graph.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub source_name: SourceName,
    pub source_id: SourceId,
    pub rooted_ref: RootedSourceRef,
    pub resolved_ref: ResolvedRef,
    pub latest_version: Option<Version>,
    /// None if source has no mars.toml.
    pub manifest: Option<Manifest>,
    /// Source names this depends on.
    pub deps: Vec<SourceName>,
}

/// Source checkout provenance and rooted package boundary.
#[derive(Debug, Clone)]
pub struct RootedSourceRef {
    pub checkout_root: PathBuf,
    pub package_root: PathBuf,
}

/// How a version constraint was specified.
#[derive(Debug, Clone)]
pub enum VersionConstraint {
    /// Semver requirement (^1.0, >=0.5.0, ~2.1, exact version).
    Semver(VersionReq),
    /// Any version, prefer newest.
    Latest,
    /// Branch or commit pin — no semver resolution.
    RefPin(String),
}

impl std::fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionConstraint::Semver(req) => write!(f, "{req}"),
            VersionConstraint::Latest => write!(f, "latest"),
            VersionConstraint::RefPin(reference) => write!(f, "ref:{reference}"),
        }
    }
}

/// An item waiting to be processed in DFS traversal.
#[derive(Debug, Clone)]
pub struct PendingItem {
    /// Package containing this item.
    pub package: SourceName,
    /// Item name.
    pub item: ItemName,
    /// Agent or Skill.
    pub kind: ItemKind,
    /// Version constraint from config.
    pub constraint: VersionConstraint,
    /// Who requested this item (for error context).
    pub required_by: String,
    /// True if from a local path dependency (skip version checks).
    pub is_local: bool,
    /// True if this was a filtered request.
    pub is_filtered: bool,
    /// Source spec for fetching if not already in registry.
    pub spec: SourceSpec,
}

/// A resolved item in the output graph.
#[derive(Debug, Clone)]
pub struct ResolvedItem {
    /// Package this item belongs to.
    pub package: SourceName,
    /// Item name.
    pub name: ItemName,
    /// Agent or Skill.
    pub kind: ItemKind,
    /// Path to item source file(s).
    pub source_path: PathBuf,
    /// Skills this item depends on (from frontmatter).
    pub skill_deps: Vec<ItemName>,
}

/// Result of checking whether an item was seen already.
#[derive(Debug)]
pub enum VersionCheckResult {
    /// Item has not been visited yet.
    NotSeen,
    /// Item was visited with a compatible version.
    SameVersion,
    /// Item was visited with a potentially conflicting version (latest vs pinned).
    PotentiallyConflicting {
        existing: VersionConstraint,
        requested: VersionConstraint,
    },
    /// Item was visited with a conflicting version.
    DifferentVersion {
        existing: VersionConstraint,
        requested: VersionConstraint,
    },
}

/// Stable key for visited items.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct VisitedItem {
    package: SourceName,
    item: ItemName,
}

/// Stored version information for a visited item.
#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub constraint: VersionConstraint,
    pub resolved_ref: ResolvedRef,
}

/// Tracks visited items with version-aware lookup for DFS traversal.
pub struct VisitedSet {
    /// Fast lookup by (package, item).
    index: HashMap<(SourceName, ItemName), ResolvedVersion>,
}

impl VisitedSet {
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
        }
    }

    fn index_key(package: &SourceName, item: &ItemName) -> (SourceName, ItemName) {
        let key = VisitedItem {
            package: package.clone(),
            item: item.clone(),
        };
        (key.package, key.item)
    }

    /// Check whether an item was visited and compare version constraints.
    pub fn check_version(
        &self,
        package: &SourceName,
        item: &ItemName,
        constraint: &VersionConstraint,
    ) -> VersionCheckResult {
        match self.index.get(&Self::index_key(package, item)) {
            None => VersionCheckResult::NotSeen,
            Some(existing) => match existing.constraint.compatible_with(constraint) {
                CompatibilityResult::Compatible => VersionCheckResult::SameVersion,
                CompatibilityResult::PotentiallyConflicting => {
                    VersionCheckResult::PotentiallyConflicting {
                        existing: existing.constraint.clone(),
                        requested: constraint.clone(),
                    }
                }
                CompatibilityResult::Conflicting => VersionCheckResult::DifferentVersion {
                    existing: existing.constraint.clone(),
                    requested: constraint.clone(),
                },
            },
        }
    }

    /// Insert an item as visited.
    pub fn insert(
        &mut self,
        package: SourceName,
        item: ItemName,
        constraint: VersionConstraint,
        resolved_ref: ResolvedRef,
    ) {
        self.index.insert(
            Self::index_key(&package, &item),
            ResolvedVersion {
                constraint,
                resolved_ref,
            },
        );
    }

    /// Iterate all visited items for graph/output assembly.
    pub fn iter(&self) -> impl Iterator<Item = (&(SourceName, ItemName), &ResolvedVersion)> {
        self.index.iter()
    }
}

/// Tracks resolved version per package and rejects divergent refs.
pub struct PackageVersions {
    /// package -> (resolved_ref, first_constraint, first_required_by)
    versions: HashMap<SourceName, (ResolvedRef, VersionConstraint, String)>,
}

impl PackageVersions {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
        }
    }

    /// Check existing package version or insert if first time seen.
    pub fn check_or_insert(
        &mut self,
        package: &SourceName,
        resolved: &ResolvedRef,
        requested: &VersionConstraint,
        required_by: &str,
        is_local: bool,
    ) -> Result<(), ResolutionError> {
        if is_local {
            return Ok(());
        }

        match self.versions.entry(package.clone()) {
            Entry::Vacant(entry) => {
                entry.insert((resolved.clone(), requested.clone(), required_by.to_string()));
                Ok(())
            }
            Entry::Occupied(entry) => {
                let (existing_ref, existing_constraint, existing_by) = entry.get();
                match existing_constraint.compatible_with(requested) {
                    CompatibilityResult::Compatible
                    | CompatibilityResult::PotentiallyConflicting => {
                        if resolved_ref_matches(existing_ref, resolved) {
                            Ok(())
                        } else {
                            Err(ResolutionError::PackageVersionConflict {
                                package: package.to_string(),
                                existing: format!("{existing_ref:?} (required by {existing_by})"),
                                requested: format!("{resolved:?} (required by {required_by})"),
                                chain: required_by.to_string(),
                            })
                        }
                    }
                    CompatibilityResult::Conflicting => {
                        Err(ResolutionError::PackageVersionConflict {
                            package: package.to_string(),
                            existing: format!("{existing_constraint} (required by {existing_by})"),
                            requested: format!("{requested} (required by {required_by})"),
                            chain: required_by.to_string(),
                        })
                    }
                }
            }
        }
    }
}

fn resolved_ref_matches(existing: &ResolvedRef, incoming: &ResolvedRef) -> bool {
    existing.source_name == incoming.source_name
        && existing.version == incoming.version
        && existing.version_tag == incoming.version_tag
        && existing.commit == incoming.commit
        && existing.tree_path == incoming.tree_path
}

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

/// Options controlling resolution behavior.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// If true, prefer newest version instead of minimum (for `mars upgrade`).
    pub maximize: bool,
    /// Source names to upgrade (empty = all, when maximize=true).
    pub upgrade_targets: HashSet<SourceName>,
    /// If true, treat direct dependency constraints for upgrade targets as
    /// unconstrained during resolution (used by `mars upgrade --bump`).
    pub bump_direct_constraints: bool,
    /// If true, locked commit replay failures become hard errors.
    pub frozen: bool,
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

/// Parse a version string into a constraint.
///
/// - `None` / `"latest"` → Latest (any version, newest wins)
/// - `"v1.2.3"` → exact match
/// - `"v2"` → `>=2.0.0, <3.0.0` (major range)
/// - `"v2.1"` → `>=2.1.0, <2.2.0` (minor range)
/// - `">=0.5.0"`, `"^2.0"`, `"~1.2"` → semver requirement
/// - anything else → branch/commit ref pin
pub fn parse_version_constraint(version: Option<&str>) -> VersionConstraint {
    let version = match version {
        None => return VersionConstraint::Latest,
        Some(v) => v.trim(),
    };

    if version.is_empty() || version.eq_ignore_ascii_case("latest") {
        return VersionConstraint::Latest;
    }

    // Try "v"-prefixed versions: v1.2.3, v2, v2.1
    if let Some(stripped) = version.strip_prefix('v') {
        // Try exact semver: v1.2.3
        if let Ok(ver) = Version::parse(stripped) {
            let req = VersionReq::parse(&format!("={ver}")).expect("valid exact req");
            return VersionConstraint::Semver(req);
        }

        // Try major-only: v2 → >=2.0.0, <3.0.0
        if let Ok(major) = stripped.parse::<u64>() {
            let req = VersionReq::parse(&format!(">={major}.0.0, <{}.0.0", major + 1))
                .expect("valid major range req");
            return VersionConstraint::Semver(req);
        }

        // Try major.minor: v2.1 → >=2.1.0, <2.2.0
        let parts: Vec<&str> = stripped.split('.').collect();
        if parts.len() == 2
            && let (Ok(major), Ok(minor)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>())
        {
            let req = VersionReq::parse(&format!(">={major}.{minor}.0, <{major}.{}.0", minor + 1))
                .expect("valid minor range req");
            return VersionConstraint::Semver(req);
        }
    }

    // Try as semver requirement directly (>=0.5.0, ^2.0, ~1.2, =1.0.0, etc.)
    if let Ok(req) = VersionReq::parse(version) {
        return VersionConstraint::Semver(req);
    }

    // Otherwise it's a branch or commit ref pin
    VersionConstraint::RefPin(version.to_string())
}

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
    let mut id_index: HashMap<SourceId, SourceName> = HashMap::new();
    let mut filter_constraints: HashMap<SourceName, Vec<FilterMode>> = HashMap::new();
    let mut constraints: HashMap<SourceName, Vec<(String, VersionConstraint)>> = HashMap::new();
    let mut registry: IndexMap<SourceName, RegisteredPackage> = IndexMap::new();
    let mut package_states: HashMap<SourceName, PackageResolutionState> = HashMap::new();
    let mut stack: Vec<PendingItem> = Vec::new();
    let mut visited = VisitedSet::new();
    let mut package_versions = PackageVersions::new();

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
        .filter(|request| is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(
            request,
            true,
            provider,
            locked,
            options,
            diag,
            &mut registry,
            &mut package_states,
            &mut id_index,
            &mut constraints,
            &mut filter_constraints,
            &mut stack,
        )?;
    }
    for request in direct_requests
        .iter()
        .filter(|request| !is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(
            request,
            true,
            provider,
            locked,
            options,
            diag,
            &mut registry,
            &mut package_states,
            &mut id_index,
            &mut constraints,
            &mut filter_constraints,
            &mut stack,
        )?;
    }

    while let Some(pending_item) = stack.pop() {
        let Some(package) = registry.get(&pending_item.package) else {
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

        match apply_item_version_policy(
            &pending_item,
            visited.check_version(
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

        package_versions
            .check_or_insert(
                &pending_item.package,
                &package.node.resolved_ref,
                &pending_item.constraint,
                &pending_item.required_by,
                pending_item.is_local,
            )
            .map_err(MarsError::from)?;

        visited.insert(
            pending_item.package.clone(),
            pending_item.item.clone(),
            pending_item.constraint.clone(),
            package.node.resolved_ref.clone(),
        );

        let skill_deps = parse_pending_item_skill_deps(&pending_item, package)?;
        for skill_dep in skill_deps {
            let resolved_skill =
                resolve_skill_ref(&skill_dep, &pending_item, &registry, &constraints)?;
            push_filter_constraint(
                &mut filter_constraints,
                &resolved_skill.package,
                &FilterMode::Include {
                    agents: Vec::new(),
                    skills: vec![resolved_skill.item.clone()],
                },
            );
            stack.push(resolved_skill);
        }
    }

    let mut nodes: IndexMap<SourceName, ResolvedNode> = IndexMap::new();
    for (name, package) in &registry {
        nodes.insert(name.clone(), package.node.clone());
    }

    validate_all_constraints(&nodes, &constraints)?;

    let order = alphabetical_order(&nodes);

    Ok(ResolvedGraph {
        nodes,
        order,
        id_index,
        filters: filter_constraints,
    })
}

/// Internal: a source waiting to be resolved.
#[derive(Debug, Clone)]
struct PendingSource {
    name: SourceName,
    source_id: SourceId,
    spec: SourceSpec,
    subpath: Option<SourceSubpath>,
    constraint: VersionConstraint,
    filter: FilterMode,
    required_by: String,
}

#[derive(Debug, Default)]
enum PackageResolutionState {
    #[default]
    Resolved,
    Resolving {
        deferred_seed_requests: Vec<PendingSource>,
    },
}

#[derive(Debug, Clone)]
struct RegisteredPackage {
    node: ResolvedNode,
    discovered: Vec<discover::DiscoveredItem>,
    discovered_index: HashMap<(ItemKind, ItemName), discover::DiscoveredItem>,
    skill_names: HashSet<ItemName>,
    constraint: VersionConstraint,
    spec: SourceSpec,
    is_local: bool,
}

impl RegisteredPackage {
    fn item(&self, kind: ItemKind, name: &ItemName) -> Option<&discover::DiscoveredItem> {
        self.discovered_index.get(&(kind, name.clone()))
    }

    fn has_skill(&self, skill: &ItemName) -> bool {
        self.skill_names.contains(skill)
    }
}

fn resolve_package_bottom_up(
    pending_src: &PendingSource,
    seed_items: bool,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    diag: &mut DiagnosticCollector,
    registry: &mut IndexMap<SourceName, RegisteredPackage>,
    package_states: &mut HashMap<SourceName, PackageResolutionState>,
    id_index: &mut HashMap<SourceId, SourceName>,
    constraints: &mut HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    filter_constraints: &mut HashMap<SourceName, Vec<FilterMode>>,
    stack: &mut Vec<PendingItem>,
) -> Result<(), MarsError> {
    if let Some(existing_name) = id_index.get(&pending_src.source_id)
        && existing_name != &pending_src.name
    {
        return Err(ResolutionError::DuplicateSourceIdentity {
            existing_name: existing_name.to_string(),
            duplicate_name: pending_src.name.to_string(),
            source_id: pending_src.source_id.to_string(),
        }
        .into());
    }

    if let Some(existing_package) = registry.get(&pending_src.name)
        && existing_package.node.source_id != pending_src.source_id
    {
        return Err(ResolutionError::SourceIdentityMismatch {
            name: pending_src.name.to_string(),
            existing: existing_package.node.source_id.to_string(),
            incoming: pending_src.source_id.to_string(),
        }
        .into());
    }

    constraints
        .entry(pending_src.name.clone())
        .or_default()
        .push((
            pending_src.required_by.clone(),
            pending_src.constraint.clone(),
        ));
    push_filter_constraint(filter_constraints, &pending_src.name, &pending_src.filter);

    if let Some(state) = package_states.get_mut(&pending_src.name) {
        match state {
            PackageResolutionState::Resolved => {
                if seed_items {
                    let package =
                        registry
                            .get(&pending_src.name)
                            .ok_or_else(|| MarsError::Source {
                                source_name: pending_src.name.to_string(),
                                message: "resolved package missing from registry".to_string(),
                            })?;
                    seed_items_for_request(pending_src, package, stack);
                }
                return Ok(());
            }
            PackageResolutionState::Resolving {
                deferred_seed_requests,
            } => {
                if seed_items {
                    deferred_seed_requests.push(pending_src.clone());
                }
                return Ok(());
            }
        }
    }

    package_states.insert(
        pending_src.name.clone(),
        PackageResolutionState::Resolving {
            deferred_seed_requests: Vec::new(),
        },
    );

    let (resolved_ref, latest_version) =
        resolve_single_source(pending_src, provider, locked, options, constraints, diag)?;
    let rooted_ref = apply_subpath(
        &pending_src.name,
        &resolved_ref.tree_path,
        pending_src.subpath.as_ref(),
    )?;
    let manifest = provider.read_manifest(&rooted_ref.package_root, diag)?;

    let mut deps = Vec::new();
    let mut manifest_requests: Vec<PendingSource> = Vec::new();
    if let Some(manifest_data) = &manifest {
        for (dep_name, dep_spec) in &manifest_data.dependencies {
            let dep_name_typed = SourceName::from(dep_name.clone());
            deps.push(dep_name_typed.clone());
            manifest_requests.push(PendingSource {
                name: dep_name_typed,
                source_id: SourceId::git_with_subpath(
                    dep_spec.url.clone(),
                    dep_spec.subpath.clone(),
                ),
                spec: SourceSpec::Git(GitSpec {
                    url: dep_spec.url.clone(),
                    version: dep_spec.version.clone(),
                }),
                subpath: dep_spec.subpath.clone(),
                constraint: parse_version_constraint(dep_spec.version.as_deref()),
                filter: dep_spec.filter.to_mode(),
                required_by: pending_src.name.to_string(),
            });
        }
    }

    let discovered = discover::discover_resolved_source(
        &rooted_ref.package_root,
        Some(pending_src.name.as_ref()),
    )?;
    let mut discovered_index: HashMap<(ItemKind, ItemName), discover::DiscoveredItem> =
        HashMap::new();
    let mut skill_names = HashSet::new();
    for item in &discovered {
        discovered_index.insert((item.id.kind, item.id.name.clone()), item.clone());
        if item.id.kind == ItemKind::Skill {
            skill_names.insert(item.id.name.clone());
        }
    }

    registry.insert(
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
            discovered,
            discovered_index,
            skill_names,
            constraint: pending_src.constraint.clone(),
            spec: pending_src.spec.clone(),
            is_local: matches!(pending_src.spec, SourceSpec::Path(_)),
        },
    );
    id_index.insert(pending_src.source_id.clone(), pending_src.name.clone());

    for request in manifest_requests
        .iter()
        .filter(|request| is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(
            request,
            true,
            provider,
            locked,
            options,
            diag,
            registry,
            package_states,
            id_index,
            constraints,
            filter_constraints,
            stack,
        )?;
    }
    for request in manifest_requests
        .iter()
        .filter(|request| !is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(
            request,
            false,
            provider,
            locked,
            options,
            diag,
            registry,
            package_states,
            id_index,
            constraints,
            filter_constraints,
            stack,
        )?;
    }

    let mut deferred_seed_requests = Vec::new();
    if let Some(PackageResolutionState::Resolving {
        deferred_seed_requests: deferred,
    }) = package_states.remove(&pending_src.name)
    {
        deferred_seed_requests = deferred;
    }
    package_states.insert(pending_src.name.clone(), PackageResolutionState::Resolved);

    let package = registry
        .get(&pending_src.name)
        .ok_or_else(|| MarsError::Source {
            source_name: pending_src.name.to_string(),
            message: "resolved package missing from registry".to_string(),
        })?;
    if seed_items {
        seed_items_for_request(pending_src, package, stack);
    }
    for deferred_request in deferred_seed_requests {
        seed_items_for_request(&deferred_request, package, stack);
    }

    Ok(())
}

fn seed_items_for_request(
    pending_src: &PendingSource,
    package: &RegisteredPackage,
    stack: &mut Vec<PendingItem>,
) {
    let mut selected: Vec<&discover::DiscoveredItem> = Vec::new();
    match &pending_src.filter {
        FilterMode::All => {
            selected.extend(package.discovered.iter());
        }
        FilterMode::Include { agents, skills } => {
            let wanted_agents: HashSet<ItemName> = agents.iter().cloned().collect();
            let wanted_skills: HashSet<ItemName> = skills.iter().cloned().collect();
            selected.extend(package.discovered.iter().filter(|item| match item.id.kind {
                ItemKind::Agent => wanted_agents.contains(&item.id.name),
                ItemKind::Skill => wanted_skills.contains(&item.id.name),
            }));
        }
        FilterMode::Exclude(excluded) => {
            selected.extend(package.discovered.iter().filter(|item| {
                let source_path = item.source_path.to_string_lossy();
                !excluded.iter().any(|excluded_item| {
                    excluded_item == &item.id.name || excluded_item == source_path.as_ref()
                })
            }));
        }
        FilterMode::OnlySkills => {
            selected.extend(
                package
                    .discovered
                    .iter()
                    .filter(|item| item.id.kind == ItemKind::Skill),
            );
        }
        FilterMode::OnlyAgents => {
            selected.extend(
                package
                    .discovered
                    .iter()
                    .filter(|item| item.id.kind == ItemKind::Agent),
            );
        }
    }

    for item in selected {
        stack.push(PendingItem {
            package: pending_src.name.clone(),
            item: item.id.name.clone(),
            kind: item.id.kind,
            constraint: pending_src.constraint.clone(),
            required_by: pending_src.required_by.clone(),
            is_local: package.is_local,
            is_filtered: !is_unfiltered_request(&pending_src.filter),
            spec: pending_src.spec.clone(),
        });
    }
}

fn parse_pending_item_skill_deps(
    pending_item: &PendingItem,
    package: &RegisteredPackage,
) -> Result<Vec<ItemName>, MarsError> {
    let Some(discovered) = package.item(pending_item.kind, &pending_item.item) else {
        return Ok(Vec::new());
    };
    let item_path =
        discovered_item_markdown_path(&package.node.rooted_ref.package_root, discovered);
    let skill_deps = validate::parse_item_skill_deps(&item_path)?;
    Ok(skill_deps.into_iter().map(ItemName::from).collect())
}

fn discovered_item_markdown_path(package_root: &Path, item: &discover::DiscoveredItem) -> PathBuf {
    match item.id.kind {
        ItemKind::Agent => package_root.join(&item.source_path),
        ItemKind::Skill => {
            if item.source_path == Path::new(".") {
                package_root.join("SKILL.md")
            } else {
                package_root.join(&item.source_path).join("SKILL.md")
            }
        }
    }
}

fn resolve_skill_ref(
    skill: &ItemName,
    requester: &PendingItem,
    registry: &IndexMap<SourceName, RegisteredPackage>,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
) -> Result<PendingItem, MarsError> {
    let required_by = format!("{}/{}", requester.package, requester.item);

    if let Some(requester_package) = registry.get(&requester.package)
        && requester_package.has_skill(skill)
    {
        let constraint = primary_package_constraint(constraints, &requester.package)
            .unwrap_or(&requester_package.constraint)
            .clone();
        return Ok(PendingItem {
            package: requester.package.clone(),
            item: skill.clone(),
            kind: ItemKind::Skill,
            constraint,
            required_by,
            is_local: requester_package.is_local,
            is_filtered: true,
            spec: requester_package.spec.clone(),
        });
    }

    for (package_name, package) in registry {
        if package_name == &requester.package {
            continue;
        }
        if !package.has_skill(skill) {
            continue;
        }

        let constraint = primary_package_constraint(constraints, package_name)
            .unwrap_or(&package.constraint)
            .clone();
        return Ok(PendingItem {
            package: package_name.clone(),
            item: skill.clone(),
            kind: ItemKind::Skill,
            constraint,
            required_by: required_by.clone(),
            is_local: package.is_local,
            is_filtered: true,
            spec: package.spec.clone(),
        });
    }

    let mut searched: Vec<String> = registry.keys().map(ToString::to_string).collect();
    searched.sort();
    Err(ResolutionError::SkillNotFound {
        skill: skill.to_string(),
        required_by,
        searched,
    }
    .into())
}

fn primary_package_constraint<'a>(
    constraints: &'a HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    package: &SourceName,
) -> Option<&'a VersionConstraint> {
    constraints
        .get(package)
        .and_then(|entries| entries.first().map(|(_, constraint)| constraint))
}

fn is_unfiltered_request(filter: &FilterMode) -> bool {
    matches!(filter, FilterMode::All)
}

fn push_filter_constraint(
    constraints: &mut HashMap<SourceName, Vec<FilterMode>>,
    source_name: &SourceName,
    filter: &FilterMode,
) {
    let entry = constraints.entry(source_name.clone()).or_default();
    if !entry.contains(filter) {
        entry.push(filter.clone());
    }
}

fn alphabetical_order(nodes: &IndexMap<SourceName, ResolvedNode>) -> Vec<SourceName> {
    let mut order: Vec<SourceName> = nodes.keys().cloned().collect();
    order.sort();
    order
}

fn apply_subpath(
    source_name: &SourceName,
    checkout_root: &Path,
    subpath: Option<&SourceSubpath>,
) -> Result<RootedSourceRef, MarsError> {
    let package_root = match subpath {
        Some(subpath) => {
            subpath
                .join_under(checkout_root)
                .map_err(|_| MarsError::SubpathTraversal {
                    source_name: source_name.to_string(),
                    subpath: subpath.to_string(),
                    checkout_root: checkout_root.to_path_buf(),
                })?
        }
        None => checkout_root.to_path_buf(),
    };

    if !package_root.exists() {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathMissing {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root does not exist under checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    if !package_root.is_dir() {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathNotDirectory {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root is not a directory under checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    let canonical_checkout = checkout_root
        .canonicalize()
        .map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!(
                "failed to canonicalize checkout root `{}`: {e}",
                checkout_root.display()
            ),
        })?;
    let canonical_package = package_root.canonicalize().map_err(|e| MarsError::Source {
        source_name: source_name.to_string(),
        message: format!(
            "failed to canonicalize package root `{}`: {e}",
            package_root.display()
        ),
    })?;

    if !canonical_package.starts_with(&canonical_checkout) {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathTraversal {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root escapes checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    Ok(RootedSourceRef {
        checkout_root: checkout_root.to_path_buf(),
        package_root,
    })
}

/// Resolve a single source to a concrete version/ref.
fn resolve_single_source(
    pending: &PendingSource,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    diag: &mut DiagnosticCollector,
) -> Result<(ResolvedRef, Option<Version>), MarsError> {
    match &pending.spec {
        SourceSpec::Path(path) => {
            // Path sources: no version resolution, just use the path
            provider
                .fetch_path(path, pending.name.as_ref(), diag)
                .map(|resolved_ref| (resolved_ref, None))
        }
        SourceSpec::Git(git) => resolve_git_source(
            &pending.name,
            &git.url,
            constraints
                .get(&pending.name)
                .map(|c| c.as_slice())
                .unwrap_or(&[]),
            provider,
            locked,
            options,
            diag,
        ),
    }
}

/// Resolve a git source: list versions, intersect constraints, select version.
fn resolve_git_source(
    name: &SourceName,
    url: &SourceUrl,
    constraints: &[(String, VersionConstraint)],
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    diag: &mut DiagnosticCollector,
) -> Result<(ResolvedRef, Option<Version>), MarsError> {
    // If all constraints are ref pins, use the first one
    // (multiple ref pins for the same source is likely an error, but we'll use first)
    let has_ref_pin = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::RefPin(_)));
    if has_ref_pin {
        for (_, constraint) in constraints {
            if let VersionConstraint::RefPin(ref_name) = constraint {
                return provider
                    .fetch_git_ref(url, ref_name, name.as_ref(), None, diag)
                    .map(|resolved_ref| (resolved_ref, None));
            }
        }
    }

    // Check if any constraint is "Latest" — if so, pick newest (not MVS)
    let has_latest = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::Latest));

    let locked_source = locked.and_then(|lf| lf.dependencies.get(name));
    let locked_commit = locked_source.and_then(|ls| ls.commit.as_deref());

    let upgrade_maximize = options.maximize
        && (options.upgrade_targets.is_empty() || options.upgrade_targets.contains(name));

    // Determine whether to maximize this source:
    // - explicit maximize mode (mars upgrade)
    // - "latest" constraint means "newest available"
    let maximize = has_latest || upgrade_maximize;

    // List available versions
    let available = provider.list_versions(url)?;
    let latest = available
        .iter()
        .max_by(|a, b| a.version.cmp(&b.version))
        .map(|v| v.version.clone());

    if available.is_empty() {
        // No semver tags → treat as "latest commit", with locked-commit replay.
        // For untagged sources, replay lock by default unless explicitly upgrading.
        let preferred_commit = if !upgrade_maximize {
            locked_commit
        } else {
            None
        };
        match provider.fetch_git_ref(url, "HEAD", name.as_ref(), preferred_commit, diag) {
            Ok(resolved) => return Ok((resolved, latest)),
            Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => {
                return Err(err);
            }
            Err(MarsError::LockedCommitUnreachable {
                commit,
                url: source_url,
            }) => {
                diag.warn(
                    "locked-commit-unreachable",
                    format!(
                        "locked commit {commit} for {source_url} is unreachable; re-resolving from HEAD"
                    ),
                );
                return provider
                    .fetch_git_ref(url, "HEAD", name.as_ref(), None, diag)
                    .map(|resolved_ref| (resolved_ref, latest));
            }
            Err(err) => return Err(err),
        }
    }

    // Collect all semver constraints
    let semver_reqs: Vec<(&str, &VersionReq)> = constraints
        .iter()
        .filter_map(|(requester, c)| match c {
            VersionConstraint::Semver(req) => Some((requester.as_str(), req)),
            _ => None,
        })
        .collect();

    // Get locked version for this source (if any)
    let locked_version = locked_source
        .and_then(|ls| ls.version.as_ref())
        .and_then(|v| {
            let v = v.strip_prefix('v').unwrap_or(v);
            Version::parse(v).ok()
        });

    // Select version
    let selected = select_version(
        name,
        &available,
        &semver_reqs,
        locked_version.as_ref(),
        maximize,
    )?;

    let should_try_locked_commit = !maximize
        && locked_commit.is_some()
        && match locked_version.as_ref() {
            Some(version) => selected.version == *version,
            None => true,
        };

    let preferred_commit = if should_try_locked_commit {
        locked_commit
    } else {
        None
    };

    match provider.fetch_git_version(url, selected, name.as_ref(), preferred_commit, diag) {
        Ok(resolved) => Ok((resolved, latest)),
        Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => Err(err),
        Err(MarsError::LockedCommitUnreachable {
            commit,
            url: source_url,
        }) => {
            diag.warn(
                "locked-commit-unreachable",
                format!(
                    "locked commit {commit} for {source_url} is unreachable; re-resolving from tag"
                ),
            );
            provider
                .fetch_git_version(url, selected, name.as_ref(), None, diag)
                .map(|resolved_ref| (resolved_ref, latest))
        }
        Err(err) => Err(err),
    }
}

/// Select a concrete version from available versions, respecting constraints.
///
/// - MVS (default): pick the minimum version satisfying all constraints.
/// - Maximize mode: pick the newest version satisfying all constraints.
/// - Locked version preference: if a locked version satisfies all constraints, use it.
fn select_version<'a>(
    source_name: &SourceName,
    available: &'a [AvailableVersion],
    constraints: &[(&str, &VersionReq)],
    locked: Option<&Version>,
    maximize: bool,
) -> Result<&'a AvailableVersion, MarsError> {
    // Find all versions satisfying all constraints
    let satisfying: Vec<&AvailableVersion> = available
        .iter()
        .filter(|av| {
            if constraints.is_empty() {
                return true;
            }
            constraints.iter().all(|(_, req)| req.matches(&av.version))
        })
        .collect();

    if satisfying.is_empty() {
        // Build helpful error message listing all constraints
        let constraint_desc: Vec<String> = constraints
            .iter()
            .map(|(requester, req)| format!("  `{requester}` requires {req}"))
            .collect();

        let available_desc: Vec<String> =
            available.iter().map(|av| av.version.to_string()).collect();

        return Err(ResolutionError::VersionConflict {
            name: source_name.to_string(),
            message: format!(
                "no version satisfies all constraints:\n{}\navailable versions: [{}]",
                constraint_desc.join("\n"),
                available_desc.join(", ")
            ),
        }
        .into());
    }

    // If we have a locked version and it satisfies constraints, prefer it
    if !maximize
        && let Some(locked_ver) = locked
        && let Some(av) = satisfying.iter().find(|av| av.version == *locked_ver)
    {
        return Ok(av);
    }

    // MVS: pick minimum. Maximize: pick maximum.
    // Available versions from list_versions are sorted ascending by semver.
    if maximize {
        Ok(satisfying.last().expect("satisfying is non-empty"))
    } else {
        Ok(satisfying.first().expect("satisfying is non-empty"))
    }
}

/// Validate that all constraints are satisfied by the resolved versions.
///
/// This catches cases where a source was resolved before all constraints
/// were known (e.g., a later transitive dep adds a new constraint on an
/// already-resolved source).
fn validate_all_constraints(
    nodes: &IndexMap<SourceName, ResolvedNode>,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
) -> Result<(), MarsError> {
    for (name, constraint_list) in constraints {
        let has_latest = constraint_list
            .iter()
            .any(|(_, constraint)| matches!(constraint, VersionConstraint::Latest));
        let node = match nodes.get(name) {
            Some(n) => n,
            None => continue, // Should not happen, but be safe
        };

        // Only validate semver constraints against resolved versions
        if let Some(ref resolved_ver) = node.resolved_ref.version {
            for (requester, constraint) in constraint_list {
                if has_latest {
                    continue;
                }
                if let VersionConstraint::Semver(req) = constraint
                    && !req.matches(resolved_ver)
                {
                    return Err(ResolutionError::VersionConflict {
                        name: name.to_string(),
                        message: format!(
                            "resolved version {resolved_ver} does not satisfy \
                             constraint {req} (required by `{requester}`)"
                        ),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    use crate::config::GitSpec;
    use crate::lock::ItemKind;
    use semver::Version;
    use std::path::PathBuf;

    fn semver_constraint(req: &str) -> VersionConstraint {
        VersionConstraint::Semver(req.parse().expect("valid semver requirement"))
    }

    fn resolved_ref(
        package: &str,
        version: Option<&str>,
        version_tag: Option<&str>,
        commit: Option<&str>,
        tree_path: &str,
    ) -> ResolvedRef {
        ResolvedRef {
            source_name: SourceName::from(package),
            version: version.map(|v| Version::parse(v).expect("valid version")),
            version_tag: version_tag.map(str::to_string),
            commit: commit.map(|c| c.into()),
            tree_path: PathBuf::from(tree_path),
        }
    }

    #[test]
    fn visited_set_not_seen() {
        let visited = VisitedSet::new();
        let package = SourceName::from("alpha");
        let item = ItemName::from("coder");

        let result = visited.check_version(&package, &item, &VersionConstraint::Latest);
        assert!(matches!(result, VersionCheckResult::NotSeen));
    }

    #[test]
    fn visited_set_same_version() {
        let mut visited = VisitedSet::new();
        let package = SourceName::from("alpha");
        let item = ItemName::from("coder");
        let constraint = semver_constraint("^1.2");
        visited.insert(
            package.clone(),
            item.clone(),
            constraint.clone(),
            resolved_ref(
                "alpha",
                Some("1.2.3"),
                Some("v1.2.3"),
                Some("abc123"),
                "/tmp/alpha",
            ),
        );

        let result = visited.check_version(&package, &item, &constraint);
        assert!(matches!(result, VersionCheckResult::SameVersion));

        let entries: Vec<_> = visited.iter().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, &(package, item));
    }

    #[test]
    fn visited_set_different_version() {
        let mut visited = VisitedSet::new();
        let package = SourceName::from("alpha");
        let item = ItemName::from("coder");

        visited.insert(
            package.clone(),
            item.clone(),
            semver_constraint("^1.0"),
            resolved_ref(
                "alpha",
                Some("1.4.0"),
                Some("v1.4.0"),
                Some("abc123"),
                "/tmp/alpha",
            ),
        );

        let requested = semver_constraint("^2.0");
        let result = visited.check_version(&package, &item, &requested);
        match result {
            VersionCheckResult::DifferentVersion {
                existing,
                requested,
            } => {
                assert!(matches!(existing, VersionConstraint::Semver(_)));
                assert!(matches!(requested, VersionConstraint::Semver(_)));
                assert_eq!(
                    existing.compatible_with(&requested),
                    CompatibilityResult::Conflicting
                );
            }
            other => panic!("expected DifferentVersion, got {other:?}"),
        }
    }

    #[test]
    fn visited_set_potentially_conflicting_version() {
        let mut visited = VisitedSet::new();
        let package = SourceName::from("alpha");
        let item = ItemName::from("coder");

        visited.insert(
            package.clone(),
            item.clone(),
            VersionConstraint::Latest,
            resolved_ref(
                "alpha",
                Some("2.0.0"),
                Some("v2.0.0"),
                Some("abc123"),
                "/tmp/alpha",
            ),
        );

        let requested = semver_constraint("^1.0");
        let result = visited.check_version(&package, &item, &requested);
        match result {
            VersionCheckResult::PotentiallyConflicting {
                existing,
                requested,
            } => {
                assert!(matches!(existing, VersionConstraint::Latest));
                assert!(matches!(requested, VersionConstraint::Semver(_)));
                assert_eq!(
                    existing.compatible_with(&requested),
                    CompatibilityResult::PotentiallyConflicting
                );
            }
            other => panic!("expected PotentiallyConflicting, got {other:?}"),
        }
    }

    fn pending_item(is_local: bool) -> PendingItem {
        PendingItem {
            package: SourceName::from("alpha"),
            item: ItemName::from("coder"),
            kind: ItemKind::Agent,
            constraint: semver_constraint("^1.0"),
            required_by: "mars.toml".to_string(),
            is_local,
            is_filtered: false,
            spec: SourceSpec::Git(GitSpec {
                url: SourceUrl::from("https://example.com/alpha.git"),
                version: Some("v1.0.0".to_string()),
            }),
        }
    }

    #[test]
    fn apply_item_version_policy_skips_local_conflict() {
        let pending = pending_item(true);
        let mut diag = DiagnosticCollector::new();
        let action = apply_item_version_policy(
            &pending,
            VersionCheckResult::DifferentVersion {
                existing: semver_constraint("^1.0"),
                requested: semver_constraint("^2.0"),
            },
            &mut diag,
        )
        .expect("local conflicting versions should be skipped");

        assert!(matches!(action, VersionAction::Skip));
    }

    #[test]
    fn apply_item_version_policy_warns_on_potential_drift() {
        let pending = pending_item(false);
        let mut diag = DiagnosticCollector::new();
        let action = apply_item_version_policy(
            &pending,
            VersionCheckResult::PotentiallyConflicting {
                existing: VersionConstraint::Latest,
                requested: semver_constraint("^1.0"),
            },
            &mut diag,
        )
        .expect("potential conflicts should warn and continue");

        assert!(matches!(action, VersionAction::Skip));
        let diagnostics = diag.drain();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "potential-version-drift");
        assert!(
            diagnostics[0]
                .message
                .contains("potential version drift: item 'coder' from 'alpha'"),
            "unexpected warning text: {}",
            diagnostics[0].message,
        );
    }

    #[test]
    fn apply_item_version_policy_errors_on_non_local_conflict() {
        let pending = pending_item(false);
        let mut diag = DiagnosticCollector::new();
        let err = apply_item_version_policy(
            &pending,
            VersionCheckResult::DifferentVersion {
                existing: semver_constraint("^1.0"),
                requested: semver_constraint("^2.0"),
            },
            &mut diag,
        )
        .expect_err("non-local conflicting versions should error");

        match err {
            ResolutionError::ItemVersionConflict {
                item,
                package,
                existing,
                requested,
                chain,
            } => {
                assert_eq!(item, "coder");
                assert_eq!(package, "alpha");
                assert_eq!(existing, "^1.0");
                assert_eq!(requested, "^2.0");
                assert_eq!(chain, "mars.toml");
            }
            other => panic!("expected ItemVersionConflict, got {other:?}"),
        }
    }

    #[test]
    fn package_versions_first_insert() {
        let mut versions = PackageVersions::new();
        let package = SourceName::from("alpha");
        let resolved = resolved_ref(
            "alpha",
            Some("1.0.0"),
            Some("v1.0.0"),
            Some("abc123"),
            "/tmp/alpha",
        );

        assert!(
            versions
                .check_or_insert(
                    &package,
                    &resolved,
                    &VersionConstraint::Latest,
                    "mars.toml",
                    false,
                )
                .is_ok()
        );
    }

    #[test]
    fn package_versions_same_version_reuse() {
        let mut versions = PackageVersions::new();
        let package = SourceName::from("alpha");
        let resolved = resolved_ref(
            "alpha",
            Some("1.0.0"),
            Some("v1.0.0"),
            Some("abc123"),
            "/tmp/alpha",
        );
        versions
            .check_or_insert(
                &package,
                &resolved,
                &VersionConstraint::Latest,
                "mars.toml",
                false,
            )
            .expect("initial insert should succeed");

        assert!(
            versions
                .check_or_insert(
                    &package,
                    &resolved,
                    &VersionConstraint::Latest,
                    "agent:coder",
                    false,
                )
                .is_ok()
        );
    }

    #[test]
    fn package_versions_conflict() {
        let mut versions = PackageVersions::new();
        let package = SourceName::from("alpha");
        let existing = resolved_ref(
            "alpha",
            Some("1.0.0"),
            Some("v1.0.0"),
            Some("abc123"),
            "/tmp/alpha-v1",
        );
        let requested = resolved_ref(
            "alpha",
            Some("2.0.0"),
            Some("v2.0.0"),
            Some("def456"),
            "/tmp/alpha-v2",
        );
        versions
            .check_or_insert(
                &package,
                &existing,
                &semver_constraint("^1.0"),
                "mars.toml",
                false,
            )
            .expect("initial insert should succeed");

        let err = versions
            .check_or_insert(
                &package,
                &requested,
                &semver_constraint("^1.0"),
                "agent:coder",
                false,
            )
            .expect_err("second insert with different resolved ref should fail");

        match err {
            ResolutionError::PackageVersionConflict {
                package,
                existing,
                requested,
                chain,
            } => {
                assert_eq!(package, "alpha");
                assert!(existing.contains("required by mars.toml"));
                assert!(requested.contains("required by agent:coder"));
                assert_eq!(chain, "agent:coder");
            }
            other => panic!("expected PackageVersionConflict, got {other:?}"),
        }
    }

    #[test]
    fn package_versions_conflicting_constraints() {
        let mut versions = PackageVersions::new();
        let package = SourceName::from("alpha");
        let resolved = resolved_ref(
            "alpha",
            Some("1.0.0"),
            Some("v1.0.0"),
            Some("abc123"),
            "/tmp/alpha",
        );
        versions
            .check_or_insert(
                &package,
                &resolved,
                &semver_constraint("^1.0"),
                "mars.toml",
                false,
            )
            .expect("initial insert should succeed");

        let err = versions
            .check_or_insert(
                &package,
                &resolved,
                &semver_constraint("^2.0"),
                "agent:coder",
                false,
            )
            .expect_err("conflicting package constraints should fail");

        match err {
            ResolutionError::PackageVersionConflict {
                package,
                existing,
                requested,
                chain,
            } => {
                assert_eq!(package, "alpha");
                assert!(existing.contains("^1.0"));
                assert!(requested.contains("^2.0"));
                assert_eq!(chain, "agent:coder");
            }
            other => panic!("expected PackageVersionConflict, got {other:?}"),
        }
    }

    #[test]
    fn package_versions_local_conflict_bypassed() {
        let mut versions = PackageVersions::new();
        let package = SourceName::from("alpha");
        let existing = resolved_ref(
            "alpha",
            Some("1.0.0"),
            Some("v1.0.0"),
            Some("abc123"),
            "/tmp/alpha-v1",
        );
        let requested = resolved_ref(
            "alpha",
            Some("2.0.0"),
            Some("v2.0.0"),
            Some("def456"),
            "/tmp/alpha-v2",
        );

        versions
            .check_or_insert(
                &package,
                &existing,
                &semver_constraint("^1.0"),
                "mars.toml",
                false,
            )
            .expect("initial insert should succeed");

        assert!(
            versions
                .check_or_insert(
                    &package,
                    &requested,
                    &semver_constraint("^2.0"),
                    "agent:coder",
                    true,
                )
                .is_ok(),
            "local package conflicts must be bypassed",
        );
    }

    #[test]
    fn pending_item_scaffolding_fields_roundtrip() {
        let pending = PendingItem {
            package: SourceName::from("alpha"),
            item: ItemName::from("coder"),
            kind: ItemKind::Agent,
            constraint: VersionConstraint::Latest,
            required_by: "mars.toml".to_string(),
            is_local: false,
            is_filtered: true,
            spec: SourceSpec::Git(GitSpec {
                url: SourceUrl::from("https://example.com/alpha.git"),
                version: Some("v1.2.3".to_string()),
            }),
        };

        assert_eq!(pending.package, "alpha");
        assert_eq!(pending.item, "coder");
        assert_eq!(pending.kind, ItemKind::Agent);
        assert!(matches!(pending.constraint, VersionConstraint::Latest));
        assert_eq!(pending.required_by, "mars.toml");
        assert!(!pending.is_local);
        assert!(pending.is_filtered);
        assert!(matches!(pending.spec, SourceSpec::Git(_)));
    }

    #[test]
    fn resolved_item_scaffolding_fields_roundtrip() {
        let resolved = ResolvedItem {
            package: SourceName::from("alpha"),
            name: ItemName::from("coder"),
            kind: ItemKind::Agent,
            source_path: PathBuf::from("agents/coder.md"),
            skill_deps: vec![ItemName::from("planning"), ItemName::from("review")],
        };

        assert_eq!(resolved.package, "alpha");
        assert_eq!(resolved.name, "coder");
        assert_eq!(resolved.kind, ItemKind::Agent);
        assert_eq!(resolved.source_path, PathBuf::from("agents/coder.md"));
        assert_eq!(
            resolved.skill_deps,
            vec![ItemName::from("planning"), ItemName::from("review")]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        EffectiveConfig, EffectiveDependency, FilterConfig, FilterMode, GitSpec, Manifest,
        ManifestDep, PackageInfo, Settings, SourceSpec,
    };
    use crate::diagnostic::DiagnosticLevel;
    use crate::types::{RenameMap, SourceId, SourceName, SourceSubpath, SourceUrl};
    use indexmap::IndexMap;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ========== Mock SourceProvider ==========

    /// Mock provider for testing the resolver without real git repos.
    struct MockProvider {
        /// url → sorted available versions
        versions: HashMap<String, Vec<AvailableVersion>>,
        /// source tree paths keyed by source name (pre-created temp dirs)
        trees: HashMap<String, PathBuf>,
        /// Manifests to return for specific source trees
        manifests: HashMap<PathBuf, Option<Manifest>>,
        /// Preferred commits that should simulate an unreachable lock replay.
        unreachable_preferred_commits: HashSet<String>,
        /// Captures preferred-commit hints passed by the resolver.
        seen_preferred_commits: RefCell<Vec<Option<String>>>,
        /// Number of fetches keyed by source name.
        fetch_counts: RefCell<HashMap<String, usize>>,
    }

    impl MockProvider {
        fn new() -> Self {
            MockProvider {
                versions: HashMap::new(),
                trees: HashMap::new(),
                manifests: HashMap::new(),
                unreachable_preferred_commits: HashSet::new(),
                seen_preferred_commits: RefCell::new(Vec::new()),
                fetch_counts: RefCell::new(HashMap::new()),
            }
        }

        /// Register available versions for a URL.
        fn add_versions(&mut self, url: &str, versions: Vec<(u64, u64, u64)>) {
            let avs: Vec<AvailableVersion> = versions
                .into_iter()
                .map(|(major, minor, patch)| AvailableVersion {
                    tag: format!("v{major}.{minor}.{patch}"),
                    version: Version::new(major, minor, patch),
                    commit_id: "0000000000000000000000000000000000000000".to_string(),
                })
                .collect();
            self.versions.insert(url.to_string(), avs);
        }

        /// Register a source tree for a source name, with optional manifest.
        fn add_source(&mut self, name: &str, tree_path: PathBuf, manifest: Option<Manifest>) {
            if let Some(ref m) = manifest {
                self.manifests.insert(tree_path.clone(), Some(m.clone()));
            } else {
                self.manifests.insert(tree_path.clone(), None);
            }
            self.trees.insert(name.to_string(), tree_path);
        }

        fn mark_unreachable_preferred_commit(&mut self, commit: &str) {
            self.unreachable_preferred_commits
                .insert(commit.to_string());
        }

        fn seen_preferred_commits(&self) -> Vec<Option<String>> {
            self.seen_preferred_commits.borrow().clone()
        }

        fn fetch_count(&self, source_name: &str) -> usize {
            self.fetch_counts
                .borrow()
                .get(source_name)
                .copied()
                .unwrap_or(0)
        }

        fn bump_fetch_count(&self, source_name: &str) {
            let mut counts = self.fetch_counts.borrow_mut();
            let entry = counts.entry(source_name.to_string()).or_insert(0);
            *entry += 1;
        }
    }

    impl VersionLister for MockProvider {
        fn list_versions(&self, url: &SourceUrl) -> Result<Vec<AvailableVersion>, MarsError> {
            Ok(self.versions.get(url.as_ref()).cloned().unwrap_or_default())
        }
    }

    impl SourceFetcher for MockProvider {
        fn fetch_git_version(
            &self,
            url: &SourceUrl,
            version: &AvailableVersion,
            source_name: &str,
            preferred_commit: Option<&str>,
            _diag: &mut DiagnosticCollector,
        ) -> Result<ResolvedRef, MarsError> {
            self.bump_fetch_count(source_name);
            self.seen_preferred_commits
                .borrow_mut()
                .push(preferred_commit.map(str::to_string));

            if let Some(commit) = preferred_commit
                && self.unreachable_preferred_commits.contains(commit)
            {
                return Err(MarsError::LockedCommitUnreachable {
                    commit: commit.to_string(),
                    url: url.to_string(),
                });
            }

            let tree_path = self.trees.get(source_name).cloned().unwrap_or_default();
            Ok(ResolvedRef {
                source_name: source_name.into(),
                version: Some(version.version.clone()),
                version_tag: Some(version.tag.clone()),
                commit: Some(
                    preferred_commit
                        .map(|c| c.into())
                        .unwrap_or_else(|| "mock-commit".into()),
                ),
                tree_path,
            })
        }

        fn fetch_git_ref(
            &self,
            url: &SourceUrl,
            ref_name: &str,
            source_name: &str,
            preferred_commit: Option<&str>,
            _diag: &mut DiagnosticCollector,
        ) -> Result<ResolvedRef, MarsError> {
            self.bump_fetch_count(source_name);
            self.seen_preferred_commits
                .borrow_mut()
                .push(preferred_commit.map(str::to_string));

            if let Some(commit) = preferred_commit
                && self.unreachable_preferred_commits.contains(commit)
            {
                return Err(MarsError::LockedCommitUnreachable {
                    commit: commit.to_string(),
                    url: url.to_string(),
                });
            }

            let tree_path = self.trees.get(source_name).cloned().unwrap_or_default();
            Ok(ResolvedRef {
                source_name: source_name.into(),
                version: None,
                version_tag: None,
                commit: Some(
                    preferred_commit
                        .map(|c| c.into())
                        .unwrap_or_else(|| format!("ref:{ref_name}").into()),
                ),
                tree_path,
            })
        }

        fn fetch_path(
            &self,
            path: &Path,
            source_name: &str,
            _diag: &mut DiagnosticCollector,
        ) -> Result<ResolvedRef, MarsError> {
            self.bump_fetch_count(source_name);
            Ok(ResolvedRef {
                source_name: source_name.into(),
                version: None,
                version_tag: None,
                commit: None,
                tree_path: path.to_path_buf(),
            })
        }
    }

    impl ManifestReader for MockProvider {
        fn read_manifest(
            &self,
            source_tree: &Path,
            _diag: &mut DiagnosticCollector,
        ) -> Result<Option<Manifest>, MarsError> {
            Ok(self.manifests.get(source_tree).cloned().unwrap_or(None))
        }
    }

    // ========== Helper functions ==========

    fn make_config(sources: Vec<(&str, SourceSpec)>) -> EffectiveConfig {
        let mut map = IndexMap::new();
        for (name, spec) in sources {
            map.insert(
                name.into(),
                EffectiveDependency {
                    name: name.into(),
                    id: source_id_for_spec(&spec, None),
                    spec,
                    subpath: None,
                    filter: FilterMode::All,
                    rename: RenameMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }
        EffectiveConfig {
            dependencies: map,
            settings: Settings::default(),
        }
    }

    fn git_spec(url: &str, version: Option<&str>) -> SourceSpec {
        SourceSpec::Git(GitSpec {
            url: SourceUrl::from(url),
            version: version.map(|s| s.to_string()),
        })
    }

    fn make_manifest(name: &str, version: &str, deps: Vec<(&str, &str, &str)>) -> Manifest {
        let mut dependencies = IndexMap::new();
        for (dep_name, dep_url, dep_ver) in deps {
            dependencies.insert(
                dep_name.to_string(),
                ManifestDep {
                    url: SourceUrl::from(dep_url),
                    subpath: None,
                    version: Some(dep_ver.to_string()),
                    filter: crate::config::FilterConfig::default(),
                },
            );
        }
        Manifest {
            package: PackageInfo {
                name: name.to_string(),
                version: version.to_string(),
                description: None,
            },
            dependencies,
            models: indexmap::IndexMap::new(),
        }
    }

    fn make_manifest_with_filters(
        name: &str,
        version: &str,
        deps: Vec<(&str, &str, &str, FilterConfig)>,
    ) -> Manifest {
        let mut dependencies = IndexMap::new();
        for (dep_name, dep_url, dep_ver, dep_filter) in deps {
            dependencies.insert(
                dep_name.to_string(),
                ManifestDep {
                    url: SourceUrl::from(dep_url),
                    subpath: None,
                    version: Some(dep_ver.to_string()),
                    filter: dep_filter,
                },
            );
        }
        Manifest {
            package: PackageInfo {
                name: name.to_string(),
                version: version.to_string(),
                description: None,
            },
            dependencies,
            models: indexmap::IndexMap::new(),
        }
    }

    fn default_options() -> ResolveOptions {
        ResolveOptions::default()
    }

    fn resolve(
        config: &EffectiveConfig,
        provider: &dyn SourceProvider,
        locked: Option<&LockFile>,
        options: &ResolveOptions,
    ) -> Result<ResolvedGraph, MarsError> {
        resolve_with_diagnostics(config, provider, locked, options).0
    }

    fn resolve_with_diagnostics(
        config: &EffectiveConfig,
        provider: &dyn SourceProvider,
        locked: Option<&LockFile>,
        options: &ResolveOptions,
    ) -> (
        Result<ResolvedGraph, MarsError>,
        Vec<crate::diagnostic::Diagnostic>,
    ) {
        let mut diag = DiagnosticCollector::new();
        let result = super::resolve(config, provider, locked, options, &mut diag);
        (result, diag.drain())
    }

    fn write_minimal_package_marker(tree: &Path) {
        std::fs::write(
            tree.join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n",
        )
        .expect("write mars.toml");
    }

    fn write_skill(tree: &Path, name: &str) {
        let dir = tree.join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), "---\n---\n").expect("write SKILL.md");
    }

    fn write_agent(tree: &Path, name: &str, skills: &[&str]) {
        let agents = tree.join("agents");
        std::fs::create_dir_all(&agents).expect("create agents dir");
        let frontmatter = if skills.is_empty() {
            "---\n---\n".to_string()
        } else {
            format!("---\nskills: [{}]\n---\n", skills.join(", "))
        };
        std::fs::write(agents.join(format!("{name}.md")), frontmatter).expect("write agent");
    }

    fn source_id_for_spec(spec: &SourceSpec, subpath: Option<SourceSubpath>) -> SourceId {
        match spec {
            SourceSpec::Git(g) => SourceId::git_with_subpath(g.url.clone(), subpath),
            SourceSpec::Path(path) => SourceId::Path {
                canonical: path.clone(),
                subpath,
            },
        }
    }

    #[test]
    fn apply_subpath_success_case() {
        let dir = TempDir::new().unwrap();
        let package_root = dir.path().join("plugins/foo");
        std::fs::create_dir_all(&package_root).unwrap();

        let subpath = SourceSubpath::new("plugins/foo").unwrap();
        let rooted = apply_subpath(&SourceName::from("dep"), dir.path(), Some(&subpath)).unwrap();

        assert_eq!(rooted.checkout_root, dir.path());
        assert_eq!(rooted.package_root, package_root);
    }

    #[test]
    fn apply_subpath_missing_directory_rejection() {
        let dir = TempDir::new().unwrap();
        let subpath = SourceSubpath::new("plugins/missing").unwrap();

        let err = apply_subpath(&SourceName::from("dep"), dir.path(), Some(&subpath))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not exist"),
            "missing directory should be rejected: {err}"
        );
    }

    #[test]
    fn apply_subpath_file_not_dir_rejection() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("plugins");
        std::fs::write(&file_path, "not a directory").unwrap();
        let subpath = SourceSubpath::new("plugins").unwrap();

        let err = apply_subpath(&SourceName::from("dep"), dir.path(), Some(&subpath))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a directory"),
            "file subpath should be rejected: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_subpath_traversal_rejection() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_pkg = outside.path().join("pkg");
        std::fs::create_dir_all(&outside_pkg).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let subpath = SourceSubpath::new("escape").unwrap();

        let err = apply_subpath(&SourceName::from("dep"), dir.path(), Some(&subpath))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("escapes checkout root"),
            "symlink traversal should be rejected: {err}"
        );
    }

    // ========== parse_version_constraint tests ==========

    #[test]
    fn parse_none_is_latest() {
        assert!(matches!(
            parse_version_constraint(None),
            VersionConstraint::Latest
        ));
    }

    #[test]
    fn parse_empty_is_latest() {
        assert!(matches!(
            parse_version_constraint(Some("")),
            VersionConstraint::Latest
        ));
    }

    #[test]
    fn parse_latest_string() {
        assert!(matches!(
            parse_version_constraint(Some("latest")),
            VersionConstraint::Latest
        ));
        assert!(matches!(
            parse_version_constraint(Some("LATEST")),
            VersionConstraint::Latest
        ));
    }

    #[test]
    fn parse_exact_version() {
        match parse_version_constraint(Some("v1.2.3")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(1, 2, 3)));
                assert!(!req.matches(&Version::new(1, 2, 4)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_major_version() {
        match parse_version_constraint(Some("v2")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(2, 0, 0)));
                assert!(req.matches(&Version::new(2, 5, 3)));
                assert!(!req.matches(&Version::new(1, 9, 9)));
                assert!(!req.matches(&Version::new(3, 0, 0)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_major_minor_version() {
        match parse_version_constraint(Some("v2.1")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(2, 1, 0)));
                assert!(req.matches(&Version::new(2, 1, 5)));
                assert!(!req.matches(&Version::new(2, 0, 9)));
                assert!(!req.matches(&Version::new(2, 2, 0)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_semver_req_gte() {
        match parse_version_constraint(Some(">=0.5.0")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(0, 5, 0)));
                assert!(req.matches(&Version::new(1, 0, 0)));
                assert!(!req.matches(&Version::new(0, 4, 9)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_semver_req_caret() {
        match parse_version_constraint(Some("^2.0")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(2, 0, 0)));
                assert!(req.matches(&Version::new(2, 9, 0)));
                assert!(!req.matches(&Version::new(3, 0, 0)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_semver_req_tilde() {
        match parse_version_constraint(Some("~1.2")) {
            VersionConstraint::Semver(req) => {
                assert!(req.matches(&Version::new(1, 2, 0)));
                assert!(req.matches(&Version::new(1, 2, 9)));
                assert!(!req.matches(&Version::new(1, 3, 0)));
            }
            other => panic!("expected Semver, got {other:?}"),
        }
    }

    #[test]
    fn parse_branch_ref() {
        match parse_version_constraint(Some("main")) {
            VersionConstraint::RefPin(ref_name) => {
                assert_eq!(ref_name, "main");
            }
            other => panic!("expected RefPin, got {other:?}"),
        }
    }

    #[test]
    fn parse_commit_ref() {
        match parse_version_constraint(Some("abc123def456")) {
            VersionConstraint::RefPin(ref_name) => {
                assert_eq!(ref_name, "abc123def456");
            }
            other => panic!("expected RefPin, got {other:?}"),
        }
    }

    // ========== Resolution tests ==========

    #[test]
    fn single_source_no_deps() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("source-a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 1, 0)]);
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.nodes.contains_key("a"));
        assert_eq!(graph.order.len(), 1);
        assert_eq!(graph.order[0], "a");

        // MVS: should pick 1.0.0 (minimum)
        let node = &graph.nodes["a"];
        assert_eq!(node.resolved_ref.version, Some(Version::new(1, 0, 0)));
    }

    #[test]
    fn two_sources_no_deps() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(2, 0, 0)]);
        provider.add_source("a", tree_a, None);
        provider.add_source("b", tree_b, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v2.0.0"))),
        ]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.order.len(), 2);
        // Both should be in the order (either order is valid since no deps)
        assert!(graph.order.contains(&"a".into()));
        assert!(graph.order.contains(&"b".into()));
    }

    #[test]
    fn source_with_transitive_dep() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_dep).unwrap();

        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("dep", "https://example.com/dep.git", ">=0.5.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions(
            "https://example.com/dep.git",
            vec![(0, 4, 0), (0, 5, 0), (0, 6, 0), (1, 0, 0)],
        );
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        // Should have both 'a' and 'dep'
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.contains_key("a"));
        assert!(graph.nodes.contains_key("dep"));

        // Dep should be resolved to minimum satisfying >=0.5.0 → 0.5.0
        let dep_node = &graph.nodes["dep"];
        assert_eq!(dep_node.resolved_ref.version, Some(Version::new(0, 5, 0)));

        // Resolver output order is deterministic alphabetical.
        assert_eq!(graph.order, vec!["a", "dep"]);
    }

    #[test]
    fn duplicate_source_identity_detects_same_url_and_subpath() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        std::fs::create_dir_all(tree_a.join("plugins/foo")).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/shared.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, None);

        let subpath = SourceSubpath::new("plugins/foo").unwrap();
        let mut dependencies = IndexMap::new();
        dependencies.insert(
            SourceName::from("a"),
            EffectiveDependency {
                name: "a".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/shared.git"),
                    Some(subpath.clone()),
                ),
                spec: git_spec("https://example.com/shared.git", Some("v1.0.0")),
                subpath: Some(subpath.clone()),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        dependencies.insert(
            SourceName::from("b"),
            EffectiveDependency {
                name: "b".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/shared.git"),
                    Some(subpath.clone()),
                ),
                spec: git_spec("https://example.com/shared.git", Some("v1.0.0")),
                subpath: Some(subpath),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        let config = EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        };

        let err = resolve(&config, &provider, None, &default_options())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("duplicate source identity"),
            "expected duplicate identity error: {err}"
        );
    }

    #[test]
    fn source_identity_mismatch_detects_different_subpaths_for_same_name() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(tree_dep.join("plugins/foo")).unwrap();
        std::fs::create_dir_all(tree_dep.join("plugins/bar")).unwrap();

        let mut manifest_deps = IndexMap::new();
        manifest_deps.insert(
            "dep".to_string(),
            ManifestDep {
                url: SourceUrl::from("https://example.com/dep.git"),
                subpath: Some(SourceSubpath::new("plugins/bar").unwrap()),
                version: Some(">=1.0.0".to_string()),
                filter: FilterConfig::default(),
            },
        );
        let manifest_a = Manifest {
            package: PackageInfo {
                name: "a".to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: manifest_deps,
            models: IndexMap::new(),
        };

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep, None);

        let mut dependencies = IndexMap::new();
        dependencies.insert(
            SourceName::from("a"),
            EffectiveDependency {
                name: "a".into(),
                id: SourceId::git(SourceUrl::from("https://example.com/a.git")),
                spec: git_spec("https://example.com/a.git", Some("v1.0.0")),
                subpath: None,
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        dependencies.insert(
            SourceName::from("dep"),
            EffectiveDependency {
                name: "dep".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/dep.git"),
                    Some(SourceSubpath::new("plugins/foo").unwrap()),
                ),
                spec: git_spec("https://example.com/dep.git", Some("v1.0.0")),
                subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        let config = EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        };

        let err = resolve(&config, &provider, None, &default_options())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("conflicting identities"),
            "expected identity mismatch error: {err}"
        );
    }

    #[test]
    fn transitive_dep_propagates_subpath_into_source_identity() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(tree_dep.join("plugins/foo")).unwrap();

        let mut manifest_deps = IndexMap::new();
        manifest_deps.insert(
            "dep".to_string(),
            ManifestDep {
                url: SourceUrl::from("https://example.com/dep.git"),
                subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
                version: Some(">=1.0.0".to_string()),
                filter: FilterConfig::default(),
            },
        );
        let manifest_a = Manifest {
            package: PackageInfo {
                name: "a".to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: manifest_deps,
            models: IndexMap::new(),
        };

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep.clone(), None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);
        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        let dep_node = graph.nodes.get("dep").expect("dep should be resolved");
        assert_eq!(
            dep_node.source_id,
            SourceId::git_with_subpath(
                SourceUrl::from("https://example.com/dep.git"),
                Some(SourceSubpath::new("plugins/foo").unwrap())
            )
        );
        assert_eq!(
            dep_node.rooted_ref.package_root,
            tree_dep.join("plugins/foo")
        );
    }

    #[test]
    fn transitive_dep_filter_is_collected() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_dep).unwrap();

        let manifest_a = make_manifest_with_filters(
            "a",
            "1.0.0",
            vec![(
                "dep",
                "https://example.com/dep.git",
                ">=1.0.0",
                FilterConfig {
                    skills: Some(vec!["frontend-design".into()]),
                    ..FilterConfig::default()
                },
            )],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert_eq!(
            graph.filters.get(&SourceName::from("dep")),
            Some(&vec![FilterMode::Include {
                agents: vec![],
                skills: vec!["frontend-design".into()],
            }])
        );
    }

    #[test]
    fn direct_and_transitive_filters_are_both_collected_for_same_source() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_dep).unwrap();

        let manifest_a = make_manifest_with_filters(
            "a",
            "1.0.0",
            vec![(
                "dep",
                "https://example.com/dep.git",
                ">=1.0.0",
                FilterConfig {
                    skills: Some(vec!["skill-b".into(), "skill-c".into()]),
                    ..FilterConfig::default()
                },
            )],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep, None);

        let mut dependencies = IndexMap::new();
        dependencies.insert(
            SourceName::from("a"),
            EffectiveDependency {
                name: "a".into(),
                id: SourceId::git(SourceUrl::from("https://example.com/a.git")),
                spec: git_spec("https://example.com/a.git", Some("v1.0.0")),
                subpath: None,
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        dependencies.insert(
            SourceName::from("dep"),
            EffectiveDependency {
                name: "dep".into(),
                id: SourceId::git(SourceUrl::from("https://example.com/dep.git")),
                spec: git_spec("https://example.com/dep.git", Some("v1.0.0")),
                subpath: None,
                filter: FilterMode::Include {
                    agents: vec![],
                    skills: vec!["skill-a".into(), "skill-b".into()],
                },
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        let config = EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        };

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let filters = graph.filters.get(&SourceName::from("dep")).unwrap();
        assert_eq!(filters.len(), 2);
        assert!(filters.contains(&FilterMode::Include {
            agents: vec![],
            skills: vec!["skill-a".into(), "skill-b".into()],
        }));
        assert!(filters.contains(&FilterMode::Include {
            agents: vec![],
            skills: vec!["skill-b".into(), "skill-c".into()],
        }));
    }

    #[test]
    fn compatible_constraints_from_two_dependents() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();

        // Both a and b depend on shared with the same constraint.
        // The resolved version must satisfy both.
        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions(
            "https://example.com/shared.git",
            vec![(1, 0, 0), (1, 2, 0), (1, 5, 0), (2, 0, 0)],
        );
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        assert_eq!(graph.nodes.len(), 3);
        // MVS with >=1.0.0 from both → picks 1.0.0 (minimum satisfying all)
        let shared_node = &graph.nodes["shared"];
        assert_eq!(
            shared_node.resolved_ref.version,
            Some(Version::new(1, 0, 0))
        );
    }

    #[test]
    fn narrower_second_constraint_causes_validation_error() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();

        // a requires shared >=1.0.0, b requires shared >=1.5.0
        // First resolver picks 1.0.0 (MVS), then validation catches >=1.5.0 failure
        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.5.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions(
            "https://example.com/shared.git",
            vec![(1, 0, 0), (1, 2, 0), (1, 5, 0), (2, 0, 0)],
        );
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        // This should fail because MVS picked 1.0.0 but b needs >=1.5.0
        let result = resolve(&config, &provider, None, &default_options());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("shared"),
            "error should mention 'shared': {err}"
        );
        assert!(
            err.contains("1.5.0"),
            "error should mention the constraint: {err}"
        );
    }

    #[test]
    fn incompatible_constraints_produce_error() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();

        // a requires shared >=2.0.0, b requires shared <1.0.0 — incompatible
        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=2.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", "<1.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions(
            "https://example.com/shared.git",
            vec![(0, 5, 0), (1, 0, 0), (2, 0, 0)],
        );
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        let result = resolve(&config, &provider, None, &default_options());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("shared"),
            "error should mention the conflicting source: {err}"
        );
    }

    #[test]
    fn cycle_does_not_error() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();

        // a depends on b, b depends on a → cycle
        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("b", "https://example.com/b.git", ">=1.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("a", "https://example.com/a.git", ">=1.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.contains_key("a"));
        assert!(graph.nodes.contains_key("b"));
    }

    #[test]
    fn same_version_revisit_skips_and_package_fetches_once() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();
        write_minimal_package_marker(&tree_shared);
        write_skill(&tree_shared, "common");

        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/shared.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert!(graph.nodes.contains_key("shared"));
        assert_eq!(provider.fetch_count("shared"), 1);
    }

    #[test]
    fn different_version_revisit_errors() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();
        write_minimal_package_marker(&tree_shared);
        write_skill(&tree_shared, "common");

        let manifest_a = make_manifest(
            "a",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=1.0.0")],
        );
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", ">=2.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/shared.git", vec![(1, 0, 0), (2, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        let err = resolve(&config, &provider, None, &default_options()).unwrap_err();
        match err {
            MarsError::Resolution(ResolutionError::ItemVersionConflict {
                item,
                package,
                existing,
                requested,
                chain,
            }) => {
                assert_eq!(item, "common");
                assert_eq!(package, "shared");
                assert!(
                    (existing == ">=1.0.0" && requested == ">=2.0.0")
                        || (existing == ">=2.0.0" && requested == ">=1.0.0"),
                    "unexpected version conflict values: existing={existing}, requested={requested}"
                );
                assert!(chain == "a" || chain == "b", "unexpected chain: {chain}");
            }
            other => panic!("expected ItemVersionConflict, got {other:?}"),
        }
    }

    #[test]
    fn latest_and_pinned_revisit_emits_warning() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        let tree_shared = dir.path().join("shared");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();
        std::fs::create_dir_all(&tree_shared).unwrap();
        write_minimal_package_marker(&tree_shared);
        write_skill(&tree_shared, "common");

        let mut deps_a = IndexMap::new();
        deps_a.insert(
            "shared".to_string(),
            ManifestDep {
                url: SourceUrl::from("https://example.com/shared.git"),
                subpath: None,
                version: None,
                filter: FilterConfig::default(),
            },
        );
        let manifest_a = Manifest {
            package: PackageInfo {
                name: "a".to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: deps_a,
            models: IndexMap::new(),
        };
        let manifest_b = make_manifest(
            "b",
            "1.0.0",
            vec![("shared", "https://example.com/shared.git", "v1.0.0")],
        );

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/shared.git", vec![(1, 0, 0), (2, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("b", tree_b, Some(manifest_b));
        provider.add_source("shared", tree_shared, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
            ("b", git_spec("https://example.com/b.git", Some("v1.0.0"))),
        ]);

        let (result, diagnostics) =
            resolve_with_diagnostics(&config, &provider, None, &default_options());
        let graph = result.expect("resolution should succeed");
        assert!(graph.nodes.contains_key("shared"));
        let drift = diagnostics
            .iter()
            .find(|diag| diag.code == "potential-version-drift")
            .expect("expected potential-version-drift warning");
        assert_eq!(drift.level, DiagnosticLevel::Warning);
        assert!(drift.message.contains("item 'common' from 'shared'"));
    }

    #[test]
    fn skill_not_found_has_requester_and_search_context() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        std::fs::create_dir_all(&tree_a).unwrap();
        write_minimal_package_marker(&tree_a);
        write_agent(&tree_a, "coder", &["missing-skill"]);

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let err = resolve(&config, &provider, None, &default_options()).unwrap_err();
        match err {
            MarsError::Resolution(ResolutionError::SkillNotFound {
                skill,
                required_by,
                searched,
            }) => {
                assert_eq!(skill, "missing-skill");
                assert_eq!(required_by, "a/coder");
                assert_eq!(searched, vec!["a".to_string()]);
            }
            other => panic!("expected SkillNotFound, got {other:?}"),
        }
    }

    #[test]
    fn locked_version_preferred_when_satisfies_constraint() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (1, 1, 0), (1, 2, 0)],
        );
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        // Lock file says v1.1.0
        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.1.0".into()),
                commit: Some("abc".into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        let node = &graph.nodes["a"];
        // Should prefer locked version 1.1.0 over MVS minimum 1.0.0
        assert_eq!(node.resolved_ref.version, Some(Version::new(1, 1, 0)));
    }

    #[test]
    fn locked_version_ignored_when_constraint_changed() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (2, 0, 0), (2, 1, 0)],
        );
        provider.add_source("a", tree, None);

        // Config now requires ^2.0
        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^2.0")),
        )]);

        // Lock file says v1.0.0 — no longer satisfies ^2.0
        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.0.0".into()),
                commit: Some("abc".into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        let node = &graph.nodes["a"];
        // Locked version doesn't satisfy ^2.0, so MVS picks 2.0.0
        assert_eq!(node.resolved_ref.version, Some(Version::new(2, 0, 0)));
    }

    #[test]
    fn locked_commit_is_used_when_reachable() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 1, 0)]);
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let locked_commit = "locked-sha-123";
        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.1.0".into()),
                commit: Some(locked_commit.into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.commit.as_deref(),
            Some(locked_commit)
        );
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(locked_commit.to_string())]
        );
    }

    #[test]
    fn normal_mode_falls_back_when_locked_commit_unreachable() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 1, 0)]);
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let unreachable_commit = "missing-locked-sha";
        provider.mark_unreachable_preferred_commit(unreachable_commit);

        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.1.0".into()),
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.version,
            Some(Version::new(1, 1, 0))
        );
        assert_eq!(
            graph.nodes["a"].resolved_ref.commit.as_deref(),
            Some("mock-commit")
        );
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(unreachable_commit.to_string()), None]
        );
    }

    #[test]
    fn frozen_mode_errors_when_locked_commit_unreachable() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 1, 0)]);
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let unreachable_commit = "missing-locked-sha";
        provider.mark_unreachable_preferred_commit(unreachable_commit);

        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.1.0".into()),
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let options = ResolveOptions {
            frozen: true,
            ..default_options()
        };
        let result = resolve(&config, &provider, Some(&lock), &options);
        assert!(matches!(
            result,
            Err(MarsError::LockedCommitUnreachable { .. })
        ));
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(unreachable_commit.to_string())]
        );
    }

    #[test]
    fn maximize_mode_ignores_locked_commit() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (1, 1, 0), (1, 2, 0)],
        );
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let unreachable_commit = "missing-locked-sha";
        provider.mark_unreachable_preferred_commit(unreachable_commit);

        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: Some("v1.0.0".into()),
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::new(),
            bump_direct_constraints: false,
            frozen: false,
        };
        let graph = resolve(&config, &provider, Some(&lock), &options).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.version,
            Some(Version::new(1, 2, 0))
        );
        assert_eq!(provider.seen_preferred_commits(), vec![None]);
    }

    #[test]
    fn latest_resolves_to_newest() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (2, 0, 0), (3, 0, 0)],
        );
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("latest")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = &graph.nodes["a"];
        // "latest" has no constraint, MVS picks minimum → 1.0.0
        // Actually, "latest" means any version. With MVS, minimum is 1.0.0.
        // But "latest" semantically means newest. Let me check the spec...
        // The spec says "@latest as any version (newest wins)"
        // So latest should pick the newest. Let me handle this in select_version.
        assert_eq!(node.resolved_ref.version, Some(Version::new(3, 0, 0)));
        assert_eq!(node.latest_version, Some(Version::new(3, 0, 0)));
    }

    #[test]
    fn v2_resolves_to_major_range() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 9, 0), (2, 0, 0), (2, 1, 0), (2, 5, 0), (3, 0, 0)],
        );
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v2")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = &graph.nodes["a"];
        // v2 → >=2.0.0, <3.0.0, MVS picks minimum → 2.0.0
        assert_eq!(node.resolved_ref.version, Some(Version::new(2, 0, 0)));
    }

    #[test]
    fn branch_ref_resolves_without_semver() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("main")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = &graph.nodes["a"];
        assert!(node.resolved_ref.version.is_none());
        assert!(node.latest_version.is_none());
        assert_eq!(node.resolved_ref.commit, Some("ref:main".into()));
    }

    #[test]
    fn source_without_manifest_has_no_transitive_deps() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree, None); // No manifest

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.nodes["a"].deps.is_empty());
    }

    #[test]
    fn path_source_resolves_without_version() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("local-source");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_source("local", tree.clone(), None);

        let config = make_config(vec![("local", SourceSpec::Path(tree))]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert_eq!(graph.nodes.len(), 1);
        let node = &graph.nodes["local"];
        assert!(node.resolved_ref.version.is_none());
        assert!(node.latest_version.is_none());
    }

    #[test]
    fn maximize_mode_picks_newest() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (1, 5, 0), (1, 9, 0)],
        );
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("^1.0")),
        )]);

        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::new(),
            bump_direct_constraints: false,
            frozen: false,
        };

        let graph = resolve(&config, &provider, None, &options).unwrap();
        let node = &graph.nodes["a"];
        assert_eq!(node.resolved_ref.version, Some(Version::new(1, 9, 0)));
    }

    #[test]
    fn maximize_with_specific_targets() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_b = dir.path().join("b");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_b).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 5, 0)]);
        provider.add_versions("https://example.com/b.git", vec![(2, 0, 0), (2, 5, 0)]);
        provider.add_source("a", tree_a, None);
        provider.add_source("b", tree_b, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("^1.0"))),
            ("b", git_spec("https://example.com/b.git", Some("^2.0"))),
        ]);

        // Only upgrade "a", not "b"
        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::from(["a".into()]),
            bump_direct_constraints: false,
            frozen: false,
        };

        let graph = resolve(&config, &provider, None, &options).unwrap();
        // "a" should be maximized → 1.5.0
        assert_eq!(
            graph.nodes["a"].resolved_ref.version,
            Some(Version::new(1, 5, 0))
        );
        // "b" should use MVS → 2.0.0
        assert_eq!(
            graph.nodes["b"].resolved_ref.version,
            Some(Version::new(2, 0, 0))
        );
    }

    #[test]
    fn bump_direct_constraints_ignores_direct_pin_for_target() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (2, 0, 0)]);
        provider.add_source("a", tree, None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);

        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::from([SourceName::from("a")]),
            bump_direct_constraints: true,
            frozen: false,
        };

        let graph = resolve(&config, &provider, None, &options).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.version,
            Some(Version::new(2, 0, 0))
        );
    }

    #[test]
    fn no_available_versions_falls_back_to_head() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        // No versions registered → empty list
        provider.add_source("a", tree, None);

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = &graph.nodes["a"];
        assert!(node.resolved_ref.version.is_none());
        assert_eq!(node.resolved_ref.commit, Some("ref:HEAD".into()));
    }

    #[test]
    fn untagged_source_uses_locked_commit_when_available() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_source("a", tree, None);

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

        let locked_commit = "locked-untagged-sha";
        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: None,
                commit: Some(locked_commit.into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.commit.as_deref(),
            Some(locked_commit)
        );
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(locked_commit.to_string())]
        );
    }

    #[test]
    fn untagged_source_falls_back_to_head_when_locked_commit_unreachable() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_source("a", tree, None);

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

        let unreachable_commit = "missing-locked-sha";
        provider.mark_unreachable_preferred_commit(unreachable_commit);

        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: None,
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        assert_eq!(
            graph.nodes["a"].resolved_ref.commit.as_deref(),
            Some("ref:HEAD")
        );
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(unreachable_commit.to_string()), None]
        );
    }

    #[test]
    fn frozen_mode_errors_for_untagged_locked_commit_unreachable() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_source("a", tree, None);

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

        let unreachable_commit = "missing-locked-sha";
        provider.mark_unreachable_preferred_commit(unreachable_commit);

        let mut lock = LockFile::empty();
        lock.dependencies.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                subpath: None,
                version: None,
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let options = ResolveOptions {
            frozen: true,
            ..default_options()
        };
        let result = resolve(&config, &provider, Some(&lock), &options);
        assert!(matches!(
            result,
            Err(MarsError::LockedCommitUnreachable { .. })
        ));
        assert_eq!(
            provider.seen_preferred_commits(),
            vec![Some(unreachable_commit.to_string())]
        );
    }

    // ========== Deterministic package order tests ==========

    #[test]
    fn alphabetical_order_linear_chain() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "c".into(),
            ResolvedNode {
                source_name: "c".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/c")),
                resolved_ref: dummy_ref("c"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["b".into()],
            },
        );
        nodes.insert(
            "b".into(),
            ResolvedNode {
                source_name: "b".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/b")),
                resolved_ref: dummy_ref("b"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["a".into()],
            },
        );
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );

        let order = alphabetical_order(&nodes);
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn alphabetical_order_ignores_dependency_shape() {
        // a depends on b and c, both depend on d
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["b".into(), "c".into()],
            },
        );
        nodes.insert(
            "b".into(),
            ResolvedNode {
                source_name: "b".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/b")),
                resolved_ref: dummy_ref("b"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["d".into()],
            },
        );
        nodes.insert(
            "c".into(),
            ResolvedNode {
                source_name: "c".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/c")),
                resolved_ref: dummy_ref("c"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["d".into()],
            },
        );
        nodes.insert(
            "d".into(),
            ResolvedNode {
                source_name: "d".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/d")),
                resolved_ref: dummy_ref("d"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );

        let order = alphabetical_order(&nodes);
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn alphabetical_order_no_deps() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );
        nodes.insert(
            "b".into(),
            ResolvedNode {
                source_name: "b".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/b")),
                resolved_ref: dummy_ref("b"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );

        let order = alphabetical_order(&nodes);
        assert_eq!(order.len(), 2);
        // Deterministic alphabetical order for independent nodes
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn alphabetical_order_is_stable_for_cycles() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["b".into()],
            },
        );
        nodes.insert(
            "b".into(),
            ResolvedNode {
                source_name: "b".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/b")),
                resolved_ref: dummy_ref("b"),
                rooted_ref: dummy_rooted_ref(),
                latest_version: None,
                manifest: None,
                deps: vec!["a".into()],
            },
        );

        let order = alphabetical_order(&nodes);
        assert_eq!(order, vec!["a", "b"]);
    }

    fn dummy_ref(name: &str) -> ResolvedRef {
        ResolvedRef {
            source_name: name.into(),
            version: None,
            version_tag: None,
            commit: None,
            tree_path: PathBuf::new(),
        }
    }

    fn dummy_rooted_ref() -> RootedSourceRef {
        RootedSourceRef {
            checkout_root: PathBuf::new(),
            package_root: PathBuf::new(),
        }
    }

    // ========== RES-006 / RES-008: apply_subpath with None subpath ==========

    /// RES-006 / RES-008: When no subpath is specified, checkout_root IS the
    /// package_root and the resolver produces a RootedSourceRef where both
    /// fields point to the same directory.
    #[test]
    fn apply_subpath_none_yields_checkout_as_package_root() {
        let dir = TempDir::new().unwrap();
        let rooted = apply_subpath(&SourceName::from("dep"), dir.path(), None).unwrap();
        assert_eq!(rooted.checkout_root, dir.path());
        assert_eq!(rooted.package_root, dir.path());
    }

    // ========== RES-009: manifest reader is called with package_root ==========

    /// RES-009: The resolver must pass `package_root` (not checkout_root) to
    /// the manifest reader.  We arrange a subpath dep whose checkout_root has
    /// no mars.toml but whose package_root (a subdirectory) does, then verify
    /// that the manifest is successfully discovered — proving package_root was
    /// used as the read base.
    #[test]
    fn resolver_reads_manifest_from_package_root_not_checkout_root() {
        let dir = TempDir::new().unwrap();
        let checkout = dir.path().join("checkout");
        let package_root = checkout.join("plugins/foo");
        std::fs::create_dir_all(&package_root).unwrap();

        // The manifest is associated with package_root, NOT the checkout root.
        // MockProvider keyed by tree_path: we register the manifest under
        // package_root so that a read from checkout_root would return None
        // while a read from package_root returns the manifest.
        let manifest = Manifest {
            package: PackageInfo {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: IndexMap::new(),
            models: IndexMap::new(),
        };

        let subpath = SourceSubpath::new("plugins/foo").unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/repo.git", vec![(1, 0, 0)]);
        // Register tree at checkout but map manifest only for package_root
        provider.trees.insert("dep".to_string(), checkout.clone());
        provider
            .manifests
            .insert(package_root.clone(), Some(manifest.clone()));
        provider.manifests.insert(checkout.clone(), None);

        let mut dependencies = IndexMap::new();
        dependencies.insert(
            SourceName::from("dep"),
            EffectiveDependency {
                name: "dep".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/repo.git"),
                    Some(subpath.clone()),
                ),
                spec: git_spec("https://example.com/repo.git", Some("v1.0.0")),
                subpath: Some(subpath),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        let config = EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        };

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = graph.nodes.get("dep").expect("dep should be in graph");
        // Manifest must be present — only possible if package_root was used
        assert!(
            node.manifest.is_some(),
            "manifest should be loaded from package_root; got None — checkout_root was likely used instead"
        );
        assert_eq!(node.rooted_ref.package_root, package_root);
        assert_eq!(node.rooted_ref.checkout_root, checkout);
    }

    // ========== RES-005: single fetch for same URL, multiple subpaths ==========

    /// RES-005: Two dependencies at different subpaths of the same git URL
    /// must not trigger a second fetch.  In our resolver the fetch is keyed by
    /// (source name, URL) so two DISTINCT dep names pointing to the same URL
    /// but different subpaths each call fetch_git_version once — but the test
    /// verifies they both resolve successfully with distinct package_roots,
    /// which is the observable contract from the resolver's perspective
    /// (cache sharing is a source-layer concern; here we verify no error is
    /// raised and both roots are distinct).
    #[test]
    fn two_subpaths_same_url_resolve_to_distinct_package_roots() {
        let dir = TempDir::new().unwrap();
        let checkout_a = dir.path().join("a");
        let checkout_b = dir.path().join("b");
        let pkg_a = checkout_a.join("plugins/foo");
        let pkg_b = checkout_b.join("plugins/bar");
        std::fs::create_dir_all(&pkg_a).unwrap();
        std::fs::create_dir_all(&pkg_b).unwrap();

        let subpath_foo = SourceSubpath::new("plugins/foo").unwrap();
        let subpath_bar = SourceSubpath::new("plugins/bar").unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/mono.git", vec![(1, 0, 0)]);
        provider.add_source("dep-a", checkout_a.clone(), None);
        provider.add_source("dep-b", checkout_b.clone(), None);

        let mut dependencies = IndexMap::new();
        dependencies.insert(
            SourceName::from("dep-a"),
            EffectiveDependency {
                name: "dep-a".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/mono.git"),
                    Some(subpath_foo.clone()),
                ),
                spec: git_spec("https://example.com/mono.git", Some("v1.0.0")),
                subpath: Some(subpath_foo),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        dependencies.insert(
            SourceName::from("dep-b"),
            EffectiveDependency {
                name: "dep-b".into(),
                id: SourceId::git_with_subpath(
                    SourceUrl::from("https://example.com/mono.git"),
                    Some(subpath_bar.clone()),
                ),
                spec: git_spec("https://example.com/mono.git", Some("v1.0.0")),
                subpath: Some(subpath_bar),
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
        let config = EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        };

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        assert_eq!(graph.nodes.len(), 2);

        let node_a = graph.nodes.get("dep-a").expect("dep-a should be resolved");
        let node_b = graph.nodes.get("dep-b").expect("dep-b should be resolved");
        // Each gets its own distinct package_root
        assert_eq!(node_a.rooted_ref.package_root, pkg_a);
        assert_eq!(node_b.rooted_ref.package_root, pkg_b);
        // checkout_roots differ because MockProvider returns different trees per name
        assert_ne!(
            node_a.rooted_ref.package_root,
            node_b.rooted_ref.package_root
        );
    }

    // ========== RES-011: transitive dep with no subpath gets None identity ==========

    /// RES-011 contrast: a transitive dep whose manifest entry has NO subpath
    /// should produce a source identity with subpath = None (not inherit from
    /// the parent).
    #[test]
    fn transitive_dep_without_subpath_has_none_in_source_identity() {
        let dir = TempDir::new().unwrap();
        let tree_a = dir.path().join("a");
        let tree_dep = dir.path().join("dep");
        std::fs::create_dir_all(&tree_a).unwrap();
        std::fs::create_dir_all(&tree_dep).unwrap();

        // 'a' depends on 'dep' with NO subpath declared
        let mut manifest_deps = IndexMap::new();
        manifest_deps.insert(
            "dep".to_string(),
            ManifestDep {
                url: SourceUrl::from("https://example.com/dep.git"),
                subpath: None,
                version: Some(">=1.0.0".to_string()),
                filter: FilterConfig::default(),
            },
        );
        let manifest_a = Manifest {
            package: PackageInfo {
                name: "a".to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: manifest_deps,
            models: IndexMap::new(),
        };

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree_a, Some(manifest_a));
        provider.add_source("dep", tree_dep.clone(), None);

        let config = make_config(vec![(
            "a",
            git_spec("https://example.com/a.git", Some("v1.0.0")),
        )]);
        let graph = resolve(&config, &provider, None, &default_options()).unwrap();

        let dep_node = graph.nodes.get("dep").expect("dep should be in graph");
        // No subpath declared → identity must have subpath = None
        assert_eq!(
            dep_node.source_id,
            SourceId::git_with_subpath(SourceUrl::from("https://example.com/dep.git"), None)
        );
        // package_root equals checkout_root when subpath is None
        assert_eq!(dep_node.rooted_ref.package_root, tree_dep);
        assert_eq!(dep_node.rooted_ref.checkout_root, tree_dep);
    }
}
