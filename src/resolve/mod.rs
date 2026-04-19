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
use crate::error::{ConfigError, MarsError, ResolutionError};
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
    /// Deterministic alphabetical order (prompt packages don't require dependency ordering).
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
    /// Source spec for fetching if not already in registry.
    pub spec: SourceSpec,
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
            if is_item_excluded(
                &filter_constraints,
                &registry,
                &resolved_skill.package,
                resolved_skill.kind,
                &resolved_skill.item,
            ) {
                continue;
            }
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

    let seed_unfiltered_manifest_deps = seed_items && is_unfiltered_request(&pending_src.filter);
    for request in manifest_requests
        .iter()
        .filter(|request| is_unfiltered_request(&request.filter))
    {
        resolve_package_bottom_up(
            request,
            seed_unfiltered_manifest_deps,
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
            spec: pending_src.spec.clone(),
        });
    }
}

fn collect_manifest_requests(
    pending_src: &PendingSource,
    package_root: &Path,
    manifest: &Option<Manifest>,
) -> Result<Vec<PendingSource>, MarsError> {
    match &pending_src.spec {
        SourceSpec::Git(_) => Ok(collect_git_manifest_requests(
            pending_src,
            manifest.as_ref(),
        )),
        SourceSpec::Path(_) => collect_path_manifest_requests(pending_src, package_root),
    }
}

fn collect_git_manifest_requests(
    pending_src: &PendingSource,
    manifest: Option<&Manifest>,
) -> Vec<PendingSource> {
    let mut requests = Vec::new();
    let Some(manifest_data) = manifest else {
        return requests;
    };
    for (dep_name, dep_spec) in &manifest_data.dependencies {
        let dep_name_typed = SourceName::from(dep_name.clone());
        requests.push(PendingSource {
            name: dep_name_typed,
            source_id: SourceId::git_with_subpath(dep_spec.url.clone(), dep_spec.subpath.clone()),
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
    requests
}

fn collect_path_manifest_requests(
    pending_src: &PendingSource,
    package_root: &Path,
) -> Result<Vec<PendingSource>, MarsError> {
    let config = match crate::config::load(package_root) {
        Ok(config) => config,
        Err(MarsError::Config(ConfigError::NotFound { .. })) => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut requests = Vec::new();
    for (dep_name, dep_spec) in config.dependencies {
        let dep_subpath = dep_spec.subpath.clone();
        let dep_filter = dep_spec.filter.to_mode();

        let (dep_spec_resolved, dep_constraint) = match (dep_spec.url, dep_spec.path) {
            (Some(url), None) => (
                SourceSpec::Git(GitSpec {
                    url,
                    version: dep_spec.version.clone(),
                }),
                parse_version_constraint(dep_spec.version.as_deref()),
            ),
            (None, Some(path)) => {
                let resolved_path = if path.is_absolute() {
                    path
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
            name: dep_name,
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

fn source_id_for_pending_spec(
    base_root: &Path,
    spec: &SourceSpec,
    subpath: Option<SourceSubpath>,
) -> SourceId {
    match spec {
        SourceSpec::Git(git) => SourceId::git_with_subpath(git.url.clone(), subpath),
        SourceSpec::Path(path) => {
            match SourceId::path_with_subpath(base_root, path, subpath.clone()) {
                Ok(id) => id,
                Err(_) => {
                    let canonical = if path.is_absolute() {
                        path.clone()
                    } else {
                        base_root.join(path)
                    };
                    SourceId::Path { canonical, subpath }
                }
            }
        }
    }
}

fn is_item_excluded(
    filter_constraints: &HashMap<SourceName, Vec<FilterMode>>,
    registry: &IndexMap<SourceName, RegisteredPackage>,
    package: &SourceName,
    kind: ItemKind,
    item: &ItemName,
) -> bool {
    let source_path = registry
        .get(package)
        .and_then(|pkg| pkg.item(kind, item))
        .map(|discovered| discovered.source_path.to_string_lossy().into_owned());

    filter_constraints
        .get(package)
        .map(|filters| {
            filters.iter().any(|filter| match filter {
                FilterMode::Exclude(excluded) => excluded.iter().any(|excluded_item| {
                    excluded_item == item
                        || source_path
                            .as_deref()
                            .is_some_and(|path| excluded_item.as_ref() == path)
                }),
                _ => false,
            })
        })
        .unwrap_or(false)
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
mod tests;
