use std::path::Path;

use crate::error::MarsError;
use crate::lock::{LockFile, LockedItem};
use crate::sync::target::{TargetItem, TargetState};

/// The diff between current disk state and desired target state.
#[derive(Debug, Clone)]
pub struct SyncDiff {
    pub items: Vec<DiffEntry>,
}

/// A single diff entry — one of six cases from the merge matrix.
#[derive(Debug, Clone)]
pub enum DiffEntry {
    /// New item not in lock or on disk.
    Add { target: TargetItem },
    /// Source changed, local unchanged → clean update.
    Update {
        target: TargetItem,
        locked: LockedItem,
    },
    /// Source unchanged, local unchanged → skip.
    Unchanged {
        target: TargetItem,
        locked: LockedItem,
    },
    /// Source changed AND local changed → needs merge.
    Conflict {
        target: TargetItem,
        locked: LockedItem,
        local_hash: String,
    },
    /// In lock but not in target → should be removed.
    Orphan { locked: LockedItem },
    /// Local modification, source unchanged → keep local.
    LocalModified {
        target: TargetItem,
        locked: LockedItem,
        local_hash: String,
    },
}

/// Compute the diff between current disk state + lock and target state.
///
/// Uses dual checksums from the lock file:
/// - `source_checksum`: what the source provided
/// - `installed_checksum`: what mars wrote to disk
///
/// Compares current disk hash against both to determine the diff entry variant.
pub fn compute(root: &Path, lock: &LockFile, target: &TargetState) -> Result<SyncDiff, MarsError> {
    let _ = (root, lock, target);
    todo!()
}
