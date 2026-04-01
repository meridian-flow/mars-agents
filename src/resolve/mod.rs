use indexmap::IndexMap;

use crate::config::EffectiveConfig;
use crate::error::MarsError;
use crate::lock::LockFile;
use crate::manifest::Manifest;
use crate::source::{CacheDir, Fetchers, ResolvedRef};

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

/// Resolve the full dependency graph from config.
///
/// Algorithm:
/// 1. Start with user's declared sources from `EffectiveConfig`
/// 2. Fetch each source → read `mars.toml` if present → discover transitive deps
/// 3. For each dependency URL seen, intersect all version constraints
/// 4. If intersection is empty → error with clear chain
/// 5. Resolve each constraint to minimum satisfying version (Go-style MVS)
/// 6. Topological sort the final graph
/// 7. Return ordered list of sources with concrete versions
///
/// When `locked` is provided, prefer locked versions when constraints allow
/// (reproducible builds).
pub fn resolve(
    _config: &EffectiveConfig,
    _fetchers: &Fetchers,
    _cache: &CacheDir,
    _locked: Option<&LockFile>,
) -> Result<ResolvedGraph, MarsError> {
    todo!()
}
