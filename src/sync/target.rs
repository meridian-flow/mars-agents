use std::path::PathBuf;

use indexmap::IndexMap;

use crate::config::EffectiveConfig;
use crate::error::MarsError;
use crate::lock::ItemId;
use crate::resolve::ResolvedGraph;

/// What `.agents/` should look like after sync.
///
/// Built from the resolved graph with intent-based filtering applied.
#[derive(Debug, Clone)]
pub struct TargetState {
    /// Keyed by dest_path (relative to .agents/).
    pub items: IndexMap<String, TargetItem>,
}

/// A single item in the desired target state.
#[derive(Debug, Clone)]
pub struct TargetItem {
    pub id: ItemId,
    pub source_name: String,
    /// Source URL for auto-rename `{owner}_{repo}` extraction.
    pub source_url: Option<String>,
    /// Path to content in fetched source tree.
    pub source_path: PathBuf,
    /// Relative path under `.agents/` (reflects rename if any).
    pub dest_path: PathBuf,
    /// SHA-256 of source content.
    pub source_hash: String,
}

/// Rename action produced by collision detection.
#[derive(Debug, Clone)]
pub struct RenameAction {
    pub original_name: String,
    pub new_name: String,
    pub source_name: String,
}

/// Build target state: discover items per source, apply agents/skills/exclude
/// filtering, resolve skill deps from frontmatter.
pub fn build(graph: &ResolvedGraph, config: &EffectiveConfig) -> Result<TargetState, MarsError> {
    let _ = (graph, config);
    todo!()
}

/// Detect collisions on destination paths and auto-rename both with
/// `{name}__{owner}_{repo}`.
///
/// Uses `source_url` from ResolvedGraph nodes for `{owner}_{repo}` extraction.
pub fn check_collisions(
    target: &mut TargetState,
    graph: &ResolvedGraph,
    config: &EffectiveConfig,
) -> Result<Vec<RenameAction>, MarsError> {
    let _ = (target, graph, config);
    todo!()
}

/// Rewrite frontmatter skill references for renamed transitive deps.
///
/// When a collision forces a rename AND affected agents have frontmatter
/// `skills:` references to the renamed skill, mars rewrites those references
/// to point at the correct renamed version.
pub fn rewrite_skill_refs(
    target: &mut TargetState,
    renames: &[RenameAction],
    graph: &ResolvedGraph,
) -> Result<(), MarsError> {
    let _ = (target, renames, graph);
    todo!()
}
