use std::path::PathBuf;

use crate::lock::{ItemId, LockedItem};
use crate::sync::apply::SyncOptions;
use crate::sync::diff::{DiffEntry, SyncDiff};
use crate::sync::target::TargetItem;
use crate::types::{DestPath, SourceName};

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
        dest_path: DestPath,
        source_name: SourceName,
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
    KeepLocal {
        item_id: ItemId,
        dest_path: DestPath,
        source_name: SourceName,
    },
}

/// Create execution plan from diff.
///
/// `--force`: all Conflict entries become Overwrite (source wins).
/// `--dry_run`: plan is computed identically but not executed (handled by apply).
pub fn create(
    diff: &SyncDiff,
    options: &SyncOptions,
    cache_bases_dir: &std::path::Path,
) -> SyncPlan {
    let mut actions = Vec::new();

    for entry in &diff.items {
        match entry {
            DiffEntry::Add { target } => {
                actions.push(PlannedAction::Install {
                    target: target.clone(),
                });
            }

            DiffEntry::Update { target, locked: _ } => {
                actions.push(PlannedAction::Overwrite {
                    target: target.clone(),
                });
            }

            DiffEntry::Unchanged { target, locked: _ } => {
                actions.push(PlannedAction::Skip {
                    item_id: target.id.clone(),
                    dest_path: target.dest_path.clone(),
                    source_name: target.source_name.clone(),
                    reason: "unchanged",
                });
            }

            DiffEntry::Conflict {
                target,
                locked,
                local_hash: _,
            } => {
                if options.force {
                    // --force: source wins, overwrite local modifications
                    actions.push(PlannedAction::Overwrite {
                        target: target.clone(),
                    });
                } else {
                    // Three-way merge needed
                    let base_path = cache_bases_dir.join(locked.installed_checksum.as_ref());

                    // Read base content from cache (or empty if missing)
                    let base_content = std::fs::read(&base_path).unwrap_or_default();

                    // Local path is the installed dest
                    let local_path = locked.dest_path.as_path().to_path_buf();

                    actions.push(PlannedAction::Merge {
                        target: target.clone(),
                        base_content,
                        local_path,
                    });
                }
            }

            DiffEntry::Orphan { locked } => {
                actions.push(PlannedAction::Remove {
                    locked: locked.clone(),
                });
            }

            DiffEntry::LocalModified {
                target,
                locked: _,
                local_hash: _,
            } => {
                if options.force {
                    // --force: source wins even when only local changed
                    actions.push(PlannedAction::Overwrite {
                        target: target.clone(),
                    });
                } else {
                    actions.push(PlannedAction::KeepLocal {
                        item_id: target.id.clone(),
                        dest_path: target.dest_path.clone(),
                        source_name: target.source_name.clone(),
                    });
                }
            }
        }
    }

    SyncPlan { actions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash;
    use crate::lock::{ItemId, ItemKind, LockedItem};
    use crate::sync::diff::{DiffEntry, SyncDiff};
    use crate::sync::target::TargetItem;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_target(name: &str) -> TargetItem {
        TargetItem {
            id: ItemId {
                kind: ItemKind::Agent,
                name: name.into(),
            },
            source_name: "test".into(),
            source_id: crate::types::SourceId::Path {
                canonical: PathBuf::from(format!("/tmp/source/agents/{name}.md")),
            },
            source_path: PathBuf::from(format!("/tmp/source/agents/{name}.md")),
            dest_path: format!("agents/{name}.md").into(),
            source_hash: hash::hash_bytes(b"test content").into(),
            rewritten_content: None,
        }
    }

    fn make_locked(name: &str) -> LockedItem {
        LockedItem {
            source: "test".into(),
            kind: ItemKind::Agent,
            version: None,
            source_checksum: hash::hash_bytes(b"old content").into(),
            installed_checksum: hash::hash_bytes(b"old content").into(),
            dest_path: format!("agents/{name}.md").into(),
        }
    }

    fn default_options() -> SyncOptions {
        SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        }
    }

    fn force_options() -> SyncOptions {
        SyncOptions {
            force: true,
            dry_run: false,
            frozen: false,
        }
    }

    #[test]
    fn add_produces_install() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Add {
                target: make_target("new-agent"),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Install { .. }));
    }

    #[test]
    fn update_produces_overwrite() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Update {
                target: make_target("updated"),
                locked: make_locked("updated"),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));
    }

    #[test]
    fn unchanged_produces_skip() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Unchanged {
                target: make_target("stable"),
                locked: make_locked("stable"),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(
            &plan.actions[0],
            PlannedAction::Skip {
                reason: "unchanged",
                ..
            }
        ));
    }

    #[test]
    fn conflict_produces_merge() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_target("conflicted"),
                locked: make_locked("conflicted"),
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Merge { .. }));
    }

    #[test]
    fn conflict_with_force_produces_overwrite() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_target("conflicted"),
                locked: make_locked("conflicted"),
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &force_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));
    }

    #[test]
    fn orphan_produces_remove() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Orphan {
                locked: make_locked("removed"),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Remove { .. }));
    }

    #[test]
    fn local_modified_produces_keep_local() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::LocalModified {
                target: make_target("modified"),
                locked: make_locked("modified"),
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::KeepLocal { .. }));
    }

    #[test]
    fn local_modified_with_force_produces_overwrite() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::LocalModified {
                target: make_target("modified"),
                locked: make_locked("modified"),
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &force_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));
    }

    #[test]
    fn merge_reads_base_from_cache() {
        let cache_dir = TempDir::new().unwrap();
        let installed_hash = hash::hash_bytes(b"installed content");

        // Write base content to cache
        let base_path = cache_dir.path().join(&installed_hash);
        std::fs::write(&base_path, b"installed content").unwrap();

        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_target("agent"),
                locked: {
                    let mut locked = make_locked("agent");
                    locked.installed_checksum = installed_hash.into();
                    locked
                },
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        match &plan.actions[0] {
            PlannedAction::Merge { base_content, .. } => {
                assert_eq!(base_content, b"installed content");
            }
            other => panic!("expected Merge, got {other:?}"),
        }
    }

    #[test]
    fn merge_with_missing_cache_uses_empty_base() {
        let cache_dir = TempDir::new().unwrap();
        // Don't write any cache file

        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_target("agent"),
                locked: make_locked("agent"),
                local_hash: "sha256:local".into(),
            }],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        match &plan.actions[0] {
            PlannedAction::Merge { base_content, .. } => {
                assert!(
                    base_content.is_empty(),
                    "missing cache should fall back to empty base"
                );
            }
            other => panic!("expected Merge, got {other:?}"),
        }
    }

    #[test]
    fn mixed_plan() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![
                DiffEntry::Add {
                    target: make_target("new"),
                },
                DiffEntry::Update {
                    target: make_target("updated"),
                    locked: make_locked("updated"),
                },
                DiffEntry::Unchanged {
                    target: make_target("stable"),
                    locked: make_locked("stable"),
                },
                DiffEntry::Orphan {
                    locked: make_locked("removed"),
                },
            ],
        };

        let plan = create(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 4);

        assert!(matches!(&plan.actions[0], PlannedAction::Install { .. }));
        assert!(matches!(&plan.actions[1], PlannedAction::Overwrite { .. }));
        assert!(matches!(&plan.actions[2], PlannedAction::Skip { .. }));
        assert!(matches!(&plan.actions[3], PlannedAction::Remove { .. }));
    }
}
