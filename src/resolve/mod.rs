//! Dependency resolution with semver constraints.
//!
//! Algorithm:
//! 1. Fetch sources from `EffectiveConfig`
//! 2. Read `mars.toml` manifests → discover transitive deps
//! 3. Intersect version constraints across dependents
//! 4. Select concrete versions (MVS: minimum version selection)
//! 5. Topological sort (Kahn's algorithm)
//!
//! Uses `semver` crate for all version parsing. No custom version logic.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use indexmap::IndexMap;
use semver::{Version, VersionReq};

use crate::config::{EffectiveConfig, GitSpec, SourceSpec};
use crate::error::{MarsError, ResolutionError};
use crate::lock::LockFile;
use crate::manifest::Manifest;
use crate::source::{AvailableVersion, ResolvedRef};
use crate::types::{SourceId, SourceName, SourceUrl};

/// The resolved dependency graph — all sources with concrete versions.
///
/// Produced by the resolver after fetching sources, reading manifests,
/// intersecting version constraints, and topological sorting.
#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    pub nodes: IndexMap<SourceName, ResolvedNode>,
    /// Topological order (deps before dependents).
    pub order: Vec<SourceName>,
    pub id_index: HashMap<SourceId, SourceName>,
}

/// A single node in the resolved graph.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub source_name: SourceName,
    pub source_id: SourceId,
    pub resolved_ref: ResolvedRef,
    /// None if source has no mars.toml.
    pub manifest: Option<Manifest>,
    /// Source names this depends on.
    pub deps: Vec<SourceName>,
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

/// Options controlling resolution behavior.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// If true, prefer newest version instead of minimum (for `mars upgrade`).
    pub maximize: bool,
    /// Source names to upgrade (empty = all, when maximize=true).
    pub upgrade_targets: HashSet<SourceName>,
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
    ) -> Result<ResolvedRef, MarsError>;

    /// Fetch a git source at a branch/commit ref (non-semver path).
    fn fetch_git_ref(
        &self,
        url: &SourceUrl,
        ref_name: &str,
        source_name: &str,
        preferred_commit: Option<&str>,
    ) -> Result<ResolvedRef, MarsError>;

    /// Resolve a local path source into a concrete tree reference.
    fn fetch_path(&self, path: &Path, source_name: &str) -> Result<ResolvedRef, MarsError>;
}

/// Reads source manifests for transitive dependency discovery.
pub trait ManifestReader {
    fn read_manifest(&self, source_tree: &Path) -> Result<Option<Manifest>, MarsError>;
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
) -> Result<ResolvedGraph, MarsError> {
    let mut nodes: IndexMap<SourceName, ResolvedNode> = IndexMap::new();
    let mut id_index: HashMap<SourceId, SourceName> = HashMap::new();

    // Pending sources to process: (name, url_or_path, version_constraint, required_by)
    let mut pending: VecDeque<PendingSource> = VecDeque::new();

    // Track constraints per source name for intersection
    let mut constraints: HashMap<SourceName, Vec<(String, VersionConstraint)>> = HashMap::new();

    // Seed with direct dependencies from config
    for (name, source) in &config.sources {
        let constraint = match &source.spec {
            SourceSpec::Git(git) => parse_version_constraint(git.version.as_deref()),
            SourceSpec::Path(_) => VersionConstraint::Latest, // Path sources: no version
        };
        pending.push_back(PendingSource {
            name: name.clone(),
            source_id: source.id.clone(),
            spec: source.spec.clone(),
            constraint,
            required_by: "mars.toml".into(),
        });
    }

    // BFS: resolve each source, discover transitive deps
    while let Some(pending_src) = pending.pop_front() {
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

        // If already resolved, just record the additional constraint
        if let Some(existing) = nodes.get(&pending_src.name) {
            if existing.source_id != pending_src.source_id {
                return Err(ResolutionError::SourceIdentityMismatch {
                    name: pending_src.name.to_string(),
                    existing: existing.source_id.to_string(),
                    incoming: pending_src.source_id.to_string(),
                }
                .into());
            }
            constraints
                .entry(pending_src.name.clone())
                .or_default()
                .push((pending_src.required_by.clone(), pending_src.constraint));
            continue;
        }

        // Record constraint
        constraints
            .entry(pending_src.name.clone())
            .or_default()
            .push((
                pending_src.required_by.clone(),
                pending_src.constraint.clone(),
            ));

        // Resolve and fetch the source
        let resolved_ref =
            resolve_single_source(&pending_src, provider, locked, options, &constraints)?;

        // Read manifest for transitive deps
        let manifest = provider.read_manifest(&resolved_ref.tree_path)?;

        // Discover transitive dependencies
        let mut deps = Vec::new();
        if let Some(ref manifest) = manifest {
            for (dep_name, dep_spec) in &manifest.dependencies {
                deps.push(SourceName::from(dep_name.clone()));

                // Only add as pending if not already resolved
                if !nodes.contains_key(dep_name.as_str()) {
                    let dep_constraint = parse_version_constraint(Some(&dep_spec.version));
                    let dep_name_typed = SourceName::from(dep_name.clone());
                    pending.push_back(PendingSource {
                        name: dep_name_typed,
                        source_id: SourceId::git(dep_spec.url.clone()),
                        spec: SourceSpec::Git(GitSpec {
                            url: dep_spec.url.clone(),
                            version: Some(dep_spec.version.clone()),
                        }),
                        constraint: dep_constraint,
                        required_by: pending_src.name.to_string(),
                    });
                } else {
                    // Already resolved — record additional constraint for later validation
                    let dep_constraint = parse_version_constraint(Some(&dep_spec.version));
                    constraints
                        .entry(SourceName::from(dep_name.clone()))
                        .or_default()
                        .push((pending_src.name.to_string(), dep_constraint));
                }
            }
        }

        nodes.insert(
            pending_src.name.clone(),
            ResolvedNode {
                source_name: pending_src.name.clone(),
                source_id: pending_src.source_id.clone(),
                resolved_ref,
                manifest,
                deps,
            },
        );
        id_index.insert(pending_src.source_id, pending_src.name);
    }

    // Validate that all constraints are satisfied by resolved versions
    validate_all_constraints(&nodes, &constraints)?;

    // Topological sort
    let order = topological_sort(&nodes)?;

    Ok(ResolvedGraph {
        nodes,
        order,
        id_index,
    })
}

/// Internal: a source waiting to be resolved.
struct PendingSource {
    name: SourceName,
    source_id: SourceId,
    spec: SourceSpec,
    constraint: VersionConstraint,
    required_by: String,
}

/// Resolve a single source to a concrete version/ref.
fn resolve_single_source(
    pending: &PendingSource,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
) -> Result<ResolvedRef, MarsError> {
    match &pending.spec {
        SourceSpec::Path(path) => {
            // Path sources: no version resolution, just use the path
            provider.fetch_path(path, pending.name.as_ref())
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
) -> Result<ResolvedRef, MarsError> {
    // If all constraints are ref pins, use the first one
    // (multiple ref pins for the same source is likely an error, but we'll use first)
    let has_ref_pin = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::RefPin(_)));
    if has_ref_pin {
        for (_, constraint) in constraints {
            if let VersionConstraint::RefPin(ref_name) = constraint {
                return provider.fetch_git_ref(url, ref_name, name.as_ref(), None);
            }
        }
    }

    // Check if any constraint is "Latest" — if so, pick newest (not MVS)
    let has_latest = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::Latest));

    let locked_source = locked.and_then(|lf| lf.sources.get(name));
    let locked_commit = locked_source.and_then(|ls| ls.commit.as_deref());

    let upgrade_maximize = options.maximize
        && (options.upgrade_targets.is_empty() || options.upgrade_targets.contains(name));

    // Determine whether to maximize this source:
    // - explicit maximize mode (mars upgrade)
    // - "latest" constraint means "newest available"
    let maximize = has_latest || upgrade_maximize;

    // List available versions
    let available = provider.list_versions(url)?;

    if available.is_empty() {
        // No semver tags → treat as "latest commit", with locked-commit replay.
        // For untagged sources, replay lock by default unless explicitly upgrading.
        let preferred_commit = if !upgrade_maximize {
            locked_commit
        } else {
            None
        };
        match provider.fetch_git_ref(url, "HEAD", name.as_ref(), preferred_commit) {
            Ok(resolved) => return Ok(resolved),
            Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => {
                return Err(err);
            }
            Err(MarsError::LockedCommitUnreachable {
                commit,
                url: source_url,
            }) => {
                eprintln!(
                    "warning: locked commit {commit} for {source_url} is unreachable; re-resolving from HEAD"
                );
                return provider.fetch_git_ref(url, "HEAD", name.as_ref(), None);
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

    match provider.fetch_git_version(url, selected, name.as_ref(), preferred_commit) {
        Ok(resolved) => Ok(resolved),
        Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => Err(err),
        Err(MarsError::LockedCommitUnreachable {
            commit,
            url: source_url,
        }) => {
            eprintln!(
                "warning: locked commit {commit} for {source_url} is unreachable; re-resolving from tag"
            );
            provider.fetch_git_version(url, selected, name.as_ref(), None)
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
        let node = match nodes.get(name) {
            Some(n) => n,
            None => continue, // Should not happen, but be safe
        };

        // Only validate semver constraints against resolved versions
        if let Some(ref resolved_ver) = node.resolved_ref.version {
            for (requester, constraint) in constraint_list {
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

/// Topological sort using Kahn's algorithm (BFS-based).
///
/// Returns source names in dependency order (deps before dependents).
/// Errors if a cycle is detected.
fn topological_sort(
    nodes: &IndexMap<SourceName, ResolvedNode>,
) -> Result<Vec<SourceName>, MarsError> {
    // Build in-degree map
    let mut in_degree: HashMap<SourceName, usize> = HashMap::new();
    let mut adjacency: HashMap<SourceName, Vec<SourceName>> = HashMap::new();

    for (name, _) in nodes {
        in_degree.entry(name.clone()).or_insert(0);
        adjacency.entry(name.clone()).or_default();
    }

    for (name, node) in nodes {
        for dep in &node.deps {
            if nodes.contains_key(dep) {
                adjacency.entry(name.clone()).or_default();
                *in_degree.entry(dep.clone()).or_insert(0) += 0; // ensure dep exists
                // dep → name edge means name depends on dep
                // In Kahn's: in_degree[name] += 1 (name has an incoming dep edge)
                *in_degree.entry(name.clone()).or_insert(0) += 1;
                adjacency.entry(dep.clone()).or_default().push(name.clone());
            }
        }
    }

    // Start with nodes that have no dependencies (in_degree == 0)
    let mut queue: VecDeque<SourceName> = VecDeque::new();
    for (name, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(name.clone());
        }
    }

    // Sort the initial queue for deterministic output
    let mut sorted_queue: Vec<SourceName> = queue.drain(..).collect();
    sorted_queue.sort();
    queue.extend(sorted_queue);

    let mut order: Vec<SourceName> = Vec::new();

    while let Some(current) = queue.pop_front() {
        order.push(current.clone());

        // Collect and sort dependents for determinism
        if let Some(dependents) = adjacency.get(&current) {
            let mut sorted_dependents: Vec<SourceName> = dependents.clone();
            sorted_dependents.sort();

            for dependent in sorted_dependents {
                if let Some(degree) = in_degree.get_mut(&dependent) {
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(dependent);
                    }
                }
            }
        }
    }

    // If we haven't visited all nodes, there's a cycle
    if order.len() != nodes.len() {
        let unvisited: Vec<&str> = nodes
            .keys()
            .filter(|name| !order.contains(name))
            .map(|s| s.as_str())
            .collect();
        let chain = unvisited.join(" → ");
        return Err(ResolutionError::Cycle { chain }.into());
    }

    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        EffectiveConfig, EffectiveSource, FilterMode, GitSpec, Settings, SourceSpec,
    };
    use crate::manifest::{DepSpec, Manifest, PackageInfo};
    use crate::types::{RenameMap, SourceId, SourceUrl};
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
    }

    impl MockProvider {
        fn new() -> Self {
            MockProvider {
                versions: HashMap::new(),
                trees: HashMap::new(),
                manifests: HashMap::new(),
                unreachable_preferred_commits: HashSet::new(),
                seen_preferred_commits: RefCell::new(Vec::new()),
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
        ) -> Result<ResolvedRef, MarsError> {
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
        ) -> Result<ResolvedRef, MarsError> {
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

        fn fetch_path(&self, path: &Path, source_name: &str) -> Result<ResolvedRef, MarsError> {
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
        fn read_manifest(&self, source_tree: &Path) -> Result<Option<Manifest>, MarsError> {
            Ok(self.manifests.get(source_tree).cloned().unwrap_or(None))
        }
    }

    // ========== Helper functions ==========

    fn make_config(sources: Vec<(&str, SourceSpec)>) -> EffectiveConfig {
        let mut map = IndexMap::new();
        for (name, spec) in sources {
            map.insert(
                name.into(),
                EffectiveSource {
                    name: name.into(),
                    id: source_id_for_spec(&spec),
                    spec,
                    filter: FilterMode::All,
                    rename: RenameMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }
        EffectiveConfig {
            sources: map,
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
                DepSpec {
                    url: SourceUrl::from(dep_url),
                    version: dep_ver.to_string(),
                    agents: vec![],
                    skills: vec![],
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
        }
    }

    fn default_options() -> ResolveOptions {
        ResolveOptions::default()
    }

    fn source_id_for_spec(spec: &SourceSpec) -> SourceId {
        match spec {
            SourceSpec::Git(g) => SourceId::git(g.url.clone()),
            SourceSpec::Path(path) => SourceId::Path {
                canonical: path.clone(),
            },
        }
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

        // Topological order: dep before a
        let dep_pos = graph.order.iter().position(|n| n == "dep").unwrap();
        let a_pos = graph.order.iter().position(|n| n == "a").unwrap();
        assert!(dep_pos < a_pos, "dep should come before a in topo order");
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
    fn cycle_detected() {
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

        let result = resolve(&config, &provider, None, &default_options());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cycle") || err.contains("Cycle"),
            "error should mention cycle: {err}"
        );
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
                version: Some("v1.0.0".into()),
                commit: Some(unreachable_commit.into()),
                tree_hash: None,
            },
        );

        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::new(),
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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
        lock.sources.insert(
            "a".into(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".into()),
                path: None,
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

    // ========== Topological sort tests ==========

    #[test]
    fn topo_sort_linear_chain() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "c".into(),
            ResolvedNode {
                source_name: "c".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/c")),
                resolved_ref: dummy_ref("c"),
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
                manifest: None,
                deps: vec![],
            },
        );

        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_sort_diamond() {
        // a depends on b and c, both depend on d
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
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
                manifest: None,
                deps: vec![],
            },
        );

        let order = topological_sort(&nodes).unwrap();
        // d must come first, a must come last
        assert_eq!(order[0], "d");
        assert_eq!(*order.last().unwrap(), "a");
        // b and c can be in either order, but both before a
        let a_pos = order.iter().position(|n| n == "a").unwrap();
        let b_pos = order.iter().position(|n| n == "b").unwrap();
        let c_pos = order.iter().position(|n| n == "c").unwrap();
        assert!(b_pos < a_pos);
        assert!(c_pos < a_pos);
    }

    #[test]
    fn topo_sort_no_deps() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
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
                manifest: None,
                deps: vec![],
            },
        );

        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order.len(), 2);
        // Deterministic alphabetical order for independent nodes
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn topo_sort_cycle_error() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "a".into(),
            ResolvedNode {
                source_name: "a".into(),
                source_id: SourceId::git(SourceUrl::from("example.com/a")),
                resolved_ref: dummy_ref("a"),
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
                manifest: None,
                deps: vec!["a".into()],
            },
        );

        let result = topological_sort(&nodes);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cycle") || err.contains("Cycle"), "{err}");
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
}
