use std::path::PathBuf;

use crate::lock::{ItemId, LockedItem};
use crate::sync::target::TargetItem;

/// A planned set of actions to execute.
#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub actions: Vec<PlannedAction>,
}

/// A single planned action derived from a diff entry.
///
/// The plan accounts for `--force` (all conflicts become `Overwrite`)
/// and `--diff` (plan is computed but not executed).
#[derive(Debug, Clone)]
pub enum PlannedAction {
    /// Copy source content to destination.
    Install { target: TargetItem },
    /// Overwrite existing file with new source content.
    Overwrite { target: TargetItem },
    /// Skip — no changes needed.
    Skip {
        item_id: ItemId,
        reason: &'static str,
    },
    /// Three-way merge required.
    Merge {
        target: TargetItem,
        base_content: Vec<u8>,
        local_path: PathBuf,
    },
    /// Remove an orphaned item.
    Remove { locked: LockedItem },
    /// Keep the local modification.
    KeepLocal { item_id: ItemId },
}
