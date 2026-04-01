pub mod apply;
pub mod diff;
pub mod plan;
pub mod target;

use std::path::PathBuf;

use crate::error::MarsError;
use crate::source::{CacheDir, Fetchers};
use crate::sync::apply::{ApplyResult, SyncOptions};
use crate::validate::ValidationWarning;

/// Context for a sync operation.
///
/// Carries the root directory, fetchers, cache, and options.
/// Supports separating resolution root from install target for future
/// workspace support.
#[derive(Debug)]
pub struct SyncContext {
    /// `.agents/` directory — config + lock live here.
    pub root: PathBuf,
    /// Where items are installed (defaults to root).
    pub install_target: PathBuf,
    pub fetchers: Fetchers,
    pub cache: CacheDir,
    pub options: SyncOptions,
}

/// Report from a completed sync operation.
#[derive(Debug)]
pub struct SyncReport {
    pub applied: ApplyResult,
    pub pruned: Vec<apply::ActionOutcome>,
    pub warnings: Vec<ValidationWarning>,
}

impl SyncReport {
    /// Whether the sync produced any unresolved conflicts.
    pub fn has_conflicts(&self) -> bool {
        self.applied
            .outcomes
            .iter()
            .any(|o| matches!(o.action, apply::ActionTaken::Conflicted))
    }
}

/// The complete sync pipeline — 15 steps matching the feature spec.
///
/// 1. Acquire sync lock
/// 2. Read agents.toml (merged with agents.local.toml)
/// 3. Fetch/update source content
/// 4. Read manifests from each source
/// 5. Resolve dependency graph
/// 6. Build target state (intent-based filtering)
/// 7. Detect collisions + auto-rename
/// 8. Rewrite frontmatter refs for renames
/// 9. Validate skill references
/// 10. Diff current state against target
/// 11. Apply changes (merge/overwrite/copy)
/// 12. Prune orphans
/// 13. Write new agents.lock
/// 14. Release lock
/// 15. Report results
pub fn sync(ctx: &SyncContext) -> Result<SyncReport, MarsError> {
    let _ = ctx;
    todo!()
}
