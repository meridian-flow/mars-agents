use std::path::PathBuf;

use crate::diagnostic::DiagnosticCollector;
use crate::lock::{ItemId, LockedItem};
use crate::sync::diff::{DiffEntry, SyncDiff};
use crate::sync::target::TargetItem;
use crate::sync::types::SyncOptions;
use crate::types::{ContentHash, DestPath, SourceName};

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
        installed_checksum: Option<ContentHash>,
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
    _cache_bases_dir: &std::path::Path,
    diag: &mut DiagnosticCollector,
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

            DiffEntry::Unchanged { target, locked } => {
                actions.push(PlannedAction::Skip {
                    item_id: target.id.clone(),
                    dest_path: target.dest_path.clone(),
                    source_name: target.source_name.clone(),
                    installed_checksum: Some(locked.installed_checksum.clone()),
                    reason: "unchanged",
                });
            }

            DiffEntry::Conflict {
                target,
                locked: _,
                local_hash: _,
            } => {
                if !options.force {
                    diag.warn(
                        "conflict-overwrite",
                        format!(
                            "{} `{}` has local modifications — overwriting with upstream",
                            target.id.kind, target.id.name
                        ),
                    );
                }

                // Source wins: overwrite local modifications
                actions.push(PlannedAction::Overwrite {
                    target: target.clone(),
                });
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

    fn make_target_with_kind(name: &str, kind: ItemKind) -> TargetItem {
        let (source_path, dest_path) = match kind {
            ItemKind::Agent => (
                PathBuf::from(format!("/tmp/source/agents/{name}.md")),
                format!("agents/{name}.md"),
            ),
            ItemKind::Skill => (
                PathBuf::from(format!("/tmp/source/skills/{name}")),
                format!("skills/{name}"),
            ),
        };

        TargetItem {
            id: ItemId {
                kind,
                name: name.into(),
            },
            source_name: "test".into(),
            origin: crate::types::SourceOrigin::Dependency("test".into()),
            source_id: crate::types::SourceId::Path {
                canonical: source_path.clone(),
            },
            source_path,
            dest_path: dest_path.into(),
            source_hash: hash::hash_bytes(b"test content").into(),
            is_flat_skill: false,
            rewritten_content: None,
        }
    }

    fn make_target(name: &str) -> TargetItem {
        make_target_with_kind(name, ItemKind::Agent)
    }

    fn make_skill_target(name: &str) -> TargetItem {
        make_target_with_kind(name, ItemKind::Skill)
    }

    fn make_locked_with_kind(name: &str, kind: ItemKind) -> LockedItem {
        let dest_path = match kind {
            ItemKind::Agent => format!("agents/{name}.md"),
            ItemKind::Skill => format!("skills/{name}"),
        };

        LockedItem {
            source: "test".into(),
            kind,
            version: None,
            source_checksum: hash::hash_bytes(b"old content").into(),
            installed_checksum: hash::hash_bytes(b"old content").into(),
            dest_path: dest_path.into(),
        }
    }

    fn make_locked(name: &str) -> LockedItem {
        make_locked_with_kind(name, ItemKind::Agent)
    }

    fn make_skill_locked(name: &str) -> LockedItem {
        make_locked_with_kind(name, ItemKind::Skill)
    }

    fn default_options() -> SyncOptions {
        SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        }
    }

    fn force_options() -> SyncOptions {
        SyncOptions {
            force: true,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        }
    }

    fn create_plan(
        diff: &SyncDiff,
        options: &SyncOptions,
        cache_bases_dir: &std::path::Path,
    ) -> SyncPlan {
        let mut diag = DiagnosticCollector::new();
        create(diff, options, cache_bases_dir, &mut diag)
    }

    fn create_plan_with_diag(
        diff: &SyncDiff,
        options: &SyncOptions,
        cache_bases_dir: &std::path::Path,
    ) -> (SyncPlan, DiagnosticCollector) {
        let mut diag = DiagnosticCollector::new();
        let plan = create(diff, options, cache_bases_dir, &mut diag);
        (plan, diag)
    }

    #[test]
    fn add_produces_install() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Add {
                target: make_target("new-agent"),
            }],
        };

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
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

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
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

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
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
    fn conflict_produces_overwrite_and_warning() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_target("conflicted"),
                locked: make_locked("conflicted"),
                local_hash: "sha256:local".into(),
            }],
        };

        let (plan, mut diag) = create_plan_with_diag(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));

        let diagnostics = diag.drain();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "conflict-overwrite");
    }

    #[test]
    fn skill_conflict_produces_overwrite_and_warning() {
        let cache_dir = TempDir::new().unwrap();
        let diff = SyncDiff {
            items: vec![DiffEntry::Conflict {
                target: make_skill_target("planning"),
                locked: make_skill_locked("planning"),
                local_hash: "sha256:local".into(),
            }],
        };
        let mut diag = DiagnosticCollector::new();

        let plan = create(&diff, &default_options(), cache_dir.path(), &mut diag);
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));

        let diagnostics = diag.drain();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "conflict-overwrite");
        assert_eq!(
            diagnostics[0].message,
            "skill `planning` has local modifications — overwriting with upstream"
        );
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

        let plan = create_plan(&diff, &force_options(), cache_dir.path());
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

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
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

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
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

        let plan = create_plan(&diff, &force_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PlannedAction::Overwrite { .. }));
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

        let plan = create_plan(&diff, &default_options(), cache_dir.path());
        assert_eq!(plan.actions.len(), 4);

        assert!(matches!(&plan.actions[0], PlannedAction::Install { .. }));
        assert!(matches!(&plan.actions[1], PlannedAction::Overwrite { .. }));
        assert!(matches!(&plan.actions[2], PlannedAction::Skip { .. }));
        assert!(matches!(&plan.actions[3], PlannedAction::Remove { .. }));
    }
}
