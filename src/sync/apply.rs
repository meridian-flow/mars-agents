use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::lock::ItemId;
use crate::sync::diff::SyncDiff;

/// Options controlling sync behavior.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Force overwrite on conflicts (skip merge).
    pub force: bool,
    /// Compute plan but don't execute (dry run).
    pub dry_run: bool,
    /// Error if lock file would change (CI mode).
    pub frozen: bool,
}

/// The result of applying the sync plan.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub outcomes: Vec<ActionOutcome>,
}

/// What action was taken for a single item.
#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub item_id: ItemId,
    pub action: ActionTaken,
    pub dest_path: PathBuf,
    pub checksum: Option<String>,
}

/// The specific action taken.
#[derive(Debug, Clone)]
pub enum ActionTaken {
    Installed,
    Updated,
    Merged,
    Conflicted,
    Removed,
    Skipped,
    Kept,
}

/// Execute the diff, applying changes to disk.
pub fn execute(
    root: &Path,
    diff: &SyncDiff,
    options: &SyncOptions,
) -> Result<ApplyResult, MarsError> {
    let _ = (root, diff, options);
    todo!()
}

/// Prune orphans: items in old lock but not in new target.
pub fn prune_orphans(
    root: &Path,
    lock: &crate::lock::LockFile,
    target: &crate::sync::target::TargetState,
) -> Result<Vec<ActionOutcome>, MarsError> {
    let _ = (root, lock, target);
    todo!()
}
