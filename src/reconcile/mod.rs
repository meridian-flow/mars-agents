use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::types::{ContentHash, ItemKind};

pub mod fs_ops;

pub use fs_ops::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationState {
    Empty,
    File { hash: ContentHash },
    Directory { hash: ContentHash },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredState {
    CopyFile { source: PathBuf, hash: ContentHash },
    CopyDir { source: PathBuf, hash: ContentHash },
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Created,
    Updated,
    Removed,
    Skipped {
        reason: &'static str,
    },
    Conflict {
        existing: DestinationState,
        desired: DesiredState,
    },
}

/// Scan a destination to determine its current state.
pub fn scan_destination(path: &Path) -> DestinationState {
    scan_destination_checked(path).unwrap_or(DestinationState::Empty)
}

/// Reconcile a single destination path to desired state.
pub fn reconcile_one(
    dest: &Path,
    desired: DesiredState,
    force: bool,
) -> Result<ReconcileOutcome, MarsError> {
    let existing = scan_destination_checked(dest)?;

    match desired {
        DesiredState::Absent => {
            if matches!(existing, DestinationState::Empty) {
                Ok(ReconcileOutcome::Skipped {
                    reason: "already absent",
                })
            } else {
                safe_remove(dest)?;
                Ok(ReconcileOutcome::Removed)
            }
        }
        DesiredState::CopyFile { source, hash } => match existing {
            DestinationState::Empty => {
                atomic_copy_file(&source, dest)?;
                Ok(ReconcileOutcome::Created)
            }
            DestinationState::File {
                hash: existing_hash,
            } if existing_hash == hash => Ok(ReconcileOutcome::Skipped {
                reason: "already up-to-date",
            }),
            existing_state => {
                if !force {
                    return Ok(ReconcileOutcome::Conflict {
                        existing: existing_state,
                        desired: DesiredState::CopyFile { source, hash },
                    });
                }
                safe_remove(dest)?;
                atomic_copy_file(&source, dest)?;
                Ok(ReconcileOutcome::Updated)
            }
        },
        DesiredState::CopyDir { source, hash } => match existing {
            DestinationState::Empty => {
                atomic_copy_dir(&source, dest)?;
                Ok(ReconcileOutcome::Created)
            }
            DestinationState::Directory {
                hash: existing_hash,
            } if existing_hash == hash => Ok(ReconcileOutcome::Skipped {
                reason: "already up-to-date",
            }),
            existing_state => {
                if !force {
                    return Ok(ReconcileOutcome::Conflict {
                        existing: existing_state,
                        desired: DesiredState::CopyDir { source, hash },
                    });
                }
                safe_remove(dest)?;
                atomic_copy_dir(&source, dest)?;
                Ok(ReconcileOutcome::Updated)
            }
        },
    }
}

fn scan_destination_checked(path: &Path) -> Result<DestinationState, MarsError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DestinationState::Empty),
        Err(e) => return Err(e.into()),
    };

    if metadata.is_file() {
        return Ok(DestinationState::File {
            hash: content_hash(path, ItemKind::Agent)?,
        });
    }

    if metadata.is_dir() {
        return Ok(DestinationState::Directory {
            hash: content_hash(path, ItemKind::Skill)?,
        });
    }

    Ok(DestinationState::Empty)
}
