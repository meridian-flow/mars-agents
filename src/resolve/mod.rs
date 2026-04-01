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

/// The resolved dependency graph — all sources with concrete versions.
///
/// Produced by the resolver after fetching sources, reading manifests,
/// intersecting version constraints, and topological sorting.
#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    pub nodes: IndexMap<String, ResolvedNode>,
    /// Topological order (deps before dependents).
    pub order: Vec<String>,
}

/// A single node in the resolved graph.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub source_name: String,
    pub resolved_ref: ResolvedRef,
    /// None if source has no mars.toml.
    pub manifest: Option<Manifest>,
    /// Source names this depends on.
    pub deps: Vec<String>,
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
    pub upgrade_targets: HashSet<String>,
}

/// Abstraction over source operations needed by the resolver.
///
/// This trait exists so the resolver can be unit-tested with mocks
/// while the real source module provides git/path implementations.
pub trait SourceProvider {
    /// List available semver-tagged versions for a git URL.
    fn list_versions(&self, url: &str) -> Result<Vec<AvailableVersion>, MarsError>;

    /// Fetch a git source at a specific version tag, returning a ResolvedRef.
    fn fetch_git_version(
        &self,
        url: &str,
        version: &AvailableVersion,
        source_name: &str,
    ) -> Result<ResolvedRef, MarsError>;

    /// Fetch a git source at a branch or commit ref (non-semver).
    fn fetch_git_ref(
        &self,
        url: &str,
        ref_name: &str,
        source_name: &str,
    ) -> Result<ResolvedRef, MarsError>;

    /// Use a local path source directly.
    fn fetch_path(&self, path: &Path, source_name: &str) -> Result<ResolvedRef, MarsError>;

    /// Read mars.toml from a source tree. Returns None if absent.
    fn read_manifest(&self, source_tree: &Path) -> Result<Option<Manifest>, MarsError>;
}

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
            let req =
                VersionReq::parse(&format!(">={major}.{minor}.0, <{major}.{}.0", minor + 1))
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
    let mut nodes: IndexMap<String, ResolvedNode> = IndexMap::new();

    // Pending sources to process: (name, url_or_path, version_constraint, required_by)
    let mut pending: VecDeque<PendingSource> = VecDeque::new();

    // Track constraints per source name for intersection
    let mut constraints: HashMap<String, Vec<(String, VersionConstraint)>> = HashMap::new();

    // Seed with direct dependencies from config
    for (name, source) in &config.sources {
        let constraint = match &source.spec {
            SourceSpec::Git(git) => parse_version_constraint(git.version.as_deref()),
            SourceSpec::Path(_) => VersionConstraint::Latest, // Path sources: no version
        };
        pending.push_back(PendingSource {
            name: name.clone(),
            spec: source.spec.clone(),
            constraint,
            required_by: "agents.toml".to_string(),
            is_direct: true,
        });
    }

    // BFS: resolve each source, discover transitive deps
    while let Some(pending_src) = pending.pop_front() {
        // If already resolved, just record the additional constraint
        if nodes.contains_key(&pending_src.name) {
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
        let resolved_ref = resolve_single_source(
            &pending_src,
            provider,
            locked,
            options,
            &constraints,
        )?;

        // Read manifest for transitive deps
        let manifest = provider.read_manifest(&resolved_ref.tree_path)?;

        // Discover transitive dependencies
        let mut deps = Vec::new();
        if let Some(ref manifest) = manifest {
            for (dep_name, dep_spec) in &manifest.dependencies {
                deps.push(dep_name.clone());

                // Only add as pending if not already resolved
                if !nodes.contains_key(dep_name) {
                    let dep_constraint = parse_version_constraint(Some(&dep_spec.version));
                    pending.push_back(PendingSource {
                        name: dep_name.clone(),
                        spec: SourceSpec::Git(GitSpec {
                            url: dep_spec.url.clone(),
                            version: Some(dep_spec.version.clone()),
                        }),
                        constraint: dep_constraint,
                        required_by: pending_src.name.clone(),
                        is_direct: false,
                    });
                } else {
                    // Already resolved — record additional constraint for later validation
                    let dep_constraint = parse_version_constraint(Some(&dep_spec.version));
                    constraints
                        .entry(dep_name.clone())
                        .or_default()
                        .push((pending_src.name.clone(), dep_constraint));
                }
            }
        }

        nodes.insert(
            pending_src.name.clone(),
            ResolvedNode {
                source_name: pending_src.name,
                resolved_ref,
                manifest,
                deps,
            },
        );
    }

    // Validate that all constraints are satisfied by resolved versions
    validate_all_constraints(&nodes, &constraints)?;

    // Topological sort
    let order = topological_sort(&nodes)?;

    Ok(ResolvedGraph { nodes, order })
}

/// Internal: a source waiting to be resolved.
struct PendingSource {
    name: String,
    spec: SourceSpec,
    constraint: VersionConstraint,
    required_by: String,
    #[allow(dead_code)]
    is_direct: bool,
}

/// Resolve a single source to a concrete version/ref.
fn resolve_single_source(
    pending: &PendingSource,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    constraints: &HashMap<String, Vec<(String, VersionConstraint)>>,
) -> Result<ResolvedRef, MarsError> {
    match &pending.spec {
        SourceSpec::Path(path) => {
            // Path sources: no version resolution, just use the path
            provider.fetch_path(path, &pending.name)
        }
        SourceSpec::Git(git) => {
            resolve_git_source(
                &pending.name,
                &git.url,
                constraints
                    .get(&pending.name)
                    .map(|c| c.as_slice())
                    .unwrap_or(&[]),
                provider,
                locked,
                options,
            )
        }
    }
}

/// Resolve a git source: list versions, intersect constraints, select version.
fn resolve_git_source(
    name: &str,
    url: &str,
    constraints: &[(String, VersionConstraint)],
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
) -> Result<ResolvedRef, MarsError> {
    // If all constraints are ref pins, use the first one
    // (multiple ref pins for the same source is likely an error, but we'll use first)
    let has_ref_pin = constraints.iter().any(|(_, c)| matches!(c, VersionConstraint::RefPin(_)));
    if has_ref_pin {
        for (_, constraint) in constraints {
            if let VersionConstraint::RefPin(ref_name) = constraint {
                return provider.fetch_git_ref(url, ref_name, name);
            }
        }
    }

    // List available versions
    let available = provider.list_versions(url)?;

    if available.is_empty() {
        // No semver tags → treat as "latest commit"
        return provider.fetch_git_ref(url, "HEAD", name);
    }

    // Collect all semver constraints
    let semver_reqs: Vec<(&str, &VersionReq)> = constraints
        .iter()
        .filter_map(|(requester, c)| match c {
            VersionConstraint::Semver(req) => Some((requester.as_str(), req)),
            _ => None,
        })
        .collect();

    // Check if any constraint is "Latest" — if so, pick newest (not MVS)
    let has_latest = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::Latest));

    // Get locked version for this source (if any)
    let locked_version = locked
        .and_then(|lf| lf.sources.get(name))
        .and_then(|ls| ls.version.as_ref())
        .and_then(|v| {
            let v = v.strip_prefix('v').unwrap_or(v);
            Version::parse(v).ok()
        });

    // Determine whether to maximize this source:
    // - explicit maximize mode (mars upgrade)
    // - "latest" constraint means "newest available"
    let maximize = has_latest
        || (options.maximize
            && (options.upgrade_targets.is_empty() || options.upgrade_targets.contains(name)));

    // Select version
    let selected = select_version(name, &available, &semver_reqs, locked_version.as_ref(), maximize)?;

    provider.fetch_git_version(url, selected, name)
}

/// Select a concrete version from available versions, respecting constraints.
///
/// - MVS (default): pick the minimum version satisfying all constraints.
/// - Maximize mode: pick the newest version satisfying all constraints.
/// - Locked version preference: if a locked version satisfies all constraints, use it.
fn select_version<'a>(
    source_name: &str,
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

        let available_desc: Vec<String> = available.iter().map(|av| av.version.to_string()).collect();

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
    nodes: &IndexMap<String, ResolvedNode>,
    constraints: &HashMap<String, Vec<(String, VersionConstraint)>>,
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
                        name: name.clone(),
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
fn topological_sort(nodes: &IndexMap<String, ResolvedNode>) -> Result<Vec<String>, MarsError> {
    // Build in-degree map
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for (name, _) in nodes {
        in_degree.entry(name.as_str()).or_insert(0);
        adjacency.entry(name.as_str()).or_default();
    }

    for (name, node) in nodes {
        for dep in &node.deps {
            if nodes.contains_key(dep) {
                adjacency.entry(name.as_str()).or_default();
                *in_degree.entry(dep.as_str()).or_insert(0) += 0; // ensure dep exists
                // dep → name edge means name depends on dep
                // In Kahn's: in_degree[name] += 1 (name has an incoming dep edge)
                *in_degree.entry(name.as_str()).or_insert(0) += 1;
                adjacency.entry(dep.as_str()).or_default().push(name.as_str());
            }
        }
    }

    // Start with nodes that have no dependencies (in_degree == 0)
    let mut queue: VecDeque<&str> = VecDeque::new();
    for (name, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(name);
        }
    }

    // Sort the initial queue for deterministic output
    let mut sorted_queue: Vec<&str> = queue.drain(..).collect();
    sorted_queue.sort();
    queue.extend(sorted_queue);

    let mut order: Vec<String> = Vec::new();

    while let Some(current) = queue.pop_front() {
        order.push(current.to_string());

        // Collect and sort dependents for determinism
        if let Some(dependents) = adjacency.get(current) {
            let mut sorted_dependents: Vec<&str> = dependents.clone();
            sorted_dependents.sort();

            for dependent in sorted_dependents {
                if let Some(degree) = in_degree.get_mut(dependent) {
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
    use crate::config::{EffectiveConfig, EffectiveSource, FilterMode, GitSpec, Settings, SourceSpec};
    use crate::manifest::{DepSpec, Manifest, PackageInfo};
    use indexmap::IndexMap;
    use std::collections::HashMap;
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
    }

    impl MockProvider {
        fn new() -> Self {
            MockProvider {
                versions: HashMap::new(),
                trees: HashMap::new(),
                manifests: HashMap::new(),
            }
        }

        /// Register available versions for a URL.
        fn add_versions(&mut self, url: &str, versions: Vec<(u64, u64, u64)>) {
            let avs: Vec<AvailableVersion> = versions
                .into_iter()
                .map(|(major, minor, patch)| AvailableVersion {
                    tag: format!("v{major}.{minor}.{patch}"),
                    version: Version::new(major, minor, patch),
                    commit_id: git2::Oid::zero(),
                })
                .collect();
            self.versions.insert(url.to_string(), avs);
        }

        /// Register a source tree for a source name, with optional manifest.
        fn add_source(
            &mut self,
            name: &str,
            tree_path: PathBuf,
            manifest: Option<Manifest>,
        ) {
            if let Some(ref m) = manifest {
                self.manifests.insert(tree_path.clone(), Some(m.clone()));
            } else {
                self.manifests.insert(tree_path.clone(), None);
            }
            self.trees.insert(name.to_string(), tree_path);
        }
    }

    impl SourceProvider for MockProvider {
        fn list_versions(&self, url: &str) -> Result<Vec<AvailableVersion>, MarsError> {
            Ok(self.versions.get(url).cloned().unwrap_or_default())
        }

        fn fetch_git_version(
            &self,
            _url: &str,
            version: &AvailableVersion,
            source_name: &str,
        ) -> Result<ResolvedRef, MarsError> {
            let tree_path = self
                .trees
                .get(source_name)
                .cloned()
                .unwrap_or_default();
            Ok(ResolvedRef {
                source_name: source_name.to_string(),
                version: Some(version.version.clone()),
                version_tag: Some(version.tag.clone()),
                commit: Some("mock-commit".to_string()),
                tree_path,
            })
        }

        fn fetch_git_ref(
            &self,
            _url: &str,
            ref_name: &str,
            source_name: &str,
        ) -> Result<ResolvedRef, MarsError> {
            let tree_path = self
                .trees
                .get(source_name)
                .cloned()
                .unwrap_or_default();
            Ok(ResolvedRef {
                source_name: source_name.to_string(),
                version: None,
                version_tag: None,
                commit: Some(format!("ref:{ref_name}")),
                tree_path,
            })
        }

        fn fetch_path(
            &self,
            path: &Path,
            source_name: &str,
        ) -> Result<ResolvedRef, MarsError> {
            Ok(ResolvedRef {
                source_name: source_name.to_string(),
                version: None,
                version_tag: None,
                commit: None,
                tree_path: path.to_path_buf(),
            })
        }

        fn read_manifest(
            &self,
            source_tree: &Path,
        ) -> Result<Option<Manifest>, MarsError> {
            Ok(self.manifests.get(source_tree).cloned().unwrap_or(None))
        }
    }

    // ========== Helper functions ==========

    fn make_config(sources: Vec<(&str, SourceSpec)>) -> EffectiveConfig {
        let mut map = IndexMap::new();
        for (name, spec) in sources {
            map.insert(
                name.to_string(),
                EffectiveSource {
                    name: name.to_string(),
                    spec,
                    filter: FilterMode::All,
                    rename: IndexMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }
        EffectiveConfig {
            sources: map,
            settings: Settings {},
        }
    }

    fn git_spec(url: &str, version: Option<&str>) -> SourceSpec {
        SourceSpec::Git(GitSpec {
            url: url.to_string(),
            version: version.map(|s| s.to_string()),
        })
    }

    fn make_manifest(name: &str, version: &str, deps: Vec<(&str, &str, &str)>) -> Manifest {
        let mut dependencies = IndexMap::new();
        for (dep_name, dep_url, dep_ver) in deps {
            dependencies.insert(
                dep_name.to_string(),
                DepSpec {
                    url: dep_url.to_string(),
                    version: dep_ver.to_string(),
                    items: None,
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

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("^1.0")))]);

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
        assert!(graph.order.contains(&"a".to_string()));
        assert!(graph.order.contains(&"b".to_string()));
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

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("v1.0.0")))]);

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
        assert!(err.contains("shared"), "error should mention 'shared': {err}");
        assert!(err.contains("1.5.0"), "error should mention the constraint: {err}");
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

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("v1.0.0"))),
        ]);

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

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("^1.0")))]);

        // Lock file says v1.1.0
        let mut lock = LockFile::empty();
        lock.sources.insert(
            "a".to_string(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".to_string()),
                path: None,
                version: Some("v1.1.0".to_string()),
                commit: Some("abc".to_string()),
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
        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("^2.0")))]);

        // Lock file says v1.0.0 — no longer satisfies ^2.0
        let mut lock = LockFile::empty();
        lock.sources.insert(
            "a".to_string(),
            crate::lock::LockedSource {
                url: Some("https://example.com/a.git".to_string()),
                path: None,
                version: Some("v1.0.0".to_string()),
                commit: Some("abc".to_string()),
                tree_hash: None,
            },
        );

        let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
        let node = &graph.nodes["a"];
        // Locked version doesn't satisfy ^2.0, so MVS picks 2.0.0
        assert_eq!(node.resolved_ref.version, Some(Version::new(2, 0, 0)));
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

        let config =
            make_config(vec![("a", git_spec("https://example.com/a.git", Some("latest")))]);

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

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("v2")))]);

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

        let config =
            make_config(vec![("a", git_spec("https://example.com/a.git", Some("main")))]);

        let graph = resolve(&config, &provider, None, &default_options()).unwrap();
        let node = &graph.nodes["a"];
        assert!(node.resolved_ref.version.is_none());
        assert_eq!(node.resolved_ref.commit, Some("ref:main".to_string()));
    }

    #[test]
    fn source_without_manifest_has_no_transitive_deps() {
        let dir = TempDir::new().unwrap();
        let tree = dir.path().join("a");
        std::fs::create_dir_all(&tree).unwrap();

        let mut provider = MockProvider::new();
        provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
        provider.add_source("a", tree, None); // No manifest

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("v1.0.0")))]);

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

        let config = make_config(vec![("a", git_spec("https://example.com/a.git", Some("^1.0")))]);

        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::new(),
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
        provider.add_versions(
            "https://example.com/a.git",
            vec![(1, 0, 0), (1, 5, 0)],
        );
        provider.add_versions(
            "https://example.com/b.git",
            vec![(2, 0, 0), (2, 5, 0)],
        );
        provider.add_source("a", tree_a, None);
        provider.add_source("b", tree_b, None);

        let config = make_config(vec![
            ("a", git_spec("https://example.com/a.git", Some("^1.0"))),
            ("b", git_spec("https://example.com/b.git", Some("^2.0"))),
        ]);

        // Only upgrade "a", not "b"
        let options = ResolveOptions {
            maximize: true,
            upgrade_targets: HashSet::from(["a".to_string()]),
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
        assert_eq!(node.resolved_ref.commit, Some("ref:HEAD".to_string()));
    }

    // ========== Topological sort tests ==========

    #[test]
    fn topo_sort_linear_chain() {
        let mut nodes = IndexMap::new();
        nodes.insert(
            "c".to_string(),
            ResolvedNode {
                source_name: "c".to_string(),
                resolved_ref: dummy_ref("c"),
                manifest: None,
                deps: vec!["b".to_string()],
            },
        );
        nodes.insert(
            "b".to_string(),
            ResolvedNode {
                source_name: "b".to_string(),
                resolved_ref: dummy_ref("b"),
                manifest: None,
                deps: vec!["a".to_string()],
            },
        );
        nodes.insert(
            "a".to_string(),
            ResolvedNode {
                source_name: "a".to_string(),
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
            "a".to_string(),
            ResolvedNode {
                source_name: "a".to_string(),
                resolved_ref: dummy_ref("a"),
                manifest: None,
                deps: vec!["b".to_string(), "c".to_string()],
            },
        );
        nodes.insert(
            "b".to_string(),
            ResolvedNode {
                source_name: "b".to_string(),
                resolved_ref: dummy_ref("b"),
                manifest: None,
                deps: vec!["d".to_string()],
            },
        );
        nodes.insert(
            "c".to_string(),
            ResolvedNode {
                source_name: "c".to_string(),
                resolved_ref: dummy_ref("c"),
                manifest: None,
                deps: vec!["d".to_string()],
            },
        );
        nodes.insert(
            "d".to_string(),
            ResolvedNode {
                source_name: "d".to_string(),
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
            "a".to_string(),
            ResolvedNode {
                source_name: "a".to_string(),
                resolved_ref: dummy_ref("a"),
                manifest: None,
                deps: vec![],
            },
        );
        nodes.insert(
            "b".to_string(),
            ResolvedNode {
                source_name: "b".to_string(),
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
            "a".to_string(),
            ResolvedNode {
                source_name: "a".to_string(),
                resolved_ref: dummy_ref("a"),
                manifest: None,
                deps: vec!["b".to_string()],
            },
        );
        nodes.insert(
            "b".to_string(),
            ResolvedNode {
                source_name: "b".to_string(),
                resolved_ref: dummy_ref("b"),
                manifest: None,
                deps: vec!["a".to_string()],
            },
        );

        let result = topological_sort(&nodes);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cycle") || err.contains("Cycle"), "{err}");
    }

    fn dummy_ref(name: &str) -> ResolvedRef {
        ResolvedRef {
            source_name: name.to_string(),
            version: None,
            version_tag: None,
            commit: None,
            tree_path: PathBuf::new(),
        }
    }
}
