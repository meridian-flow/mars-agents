//! Target sync — copy content from .mars/ canonical store to managed targets.
//!
//! After `apply_plan()` writes resolved content to `.mars/agents/` and `.mars/skills/`,
//! this module copies that content to all configured target directories (`.agents/`, `.claude/`, etc.).
//!
//! All targets are managed outputs — they get copies (not symlinks) of .mars/ content.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::reconcile::fs_ops;
use crate::sync::apply::{ActionOutcome, ActionTaken};

/// A directory that mars manages — materialized from .mars/.
#[derive(Debug, Clone)]
pub struct ManagedTarget {
    /// Target directory path relative to project root (e.g. ".claude", ".agents").
    pub path: String,
}

/// Result of syncing content to a single target directory.
#[derive(Debug, Clone)]
pub struct TargetSyncOutcome {
    /// Target directory name (e.g. ".claude").
    pub target: String,
    /// Number of items successfully synced.
    pub items_synced: usize,
    /// Number of items removed (orphan cleanup).
    pub items_removed: usize,
    /// Non-fatal errors encountered during sync.
    pub errors: Vec<String>,
}

/// Sync all managed targets from .mars/ canonical store.
///
/// For each configured target, copies content from `.mars/agents/` and `.mars/skills/`
/// into the target directory.
/// Cleans up orphaned items that are no longer in the apply outcomes.
///
/// Target sync is non-fatal by default (D9) — errors per-target are recorded but don't
/// stop other targets from being synced.
pub fn sync_managed_targets(
    project_root: &Path,
    mars_dir: &Path,
    targets: &[String],
    outcomes: &[ActionOutcome],
    previous_managed_paths: &HashSet<PathBuf>,
    force: bool,
    diag: &mut DiagnosticCollector,
) -> Vec<TargetSyncOutcome> {
    let mut results = Vec::new();

    for target_name in targets {
        let target_root = project_root.join(target_name);
        match sync_one_target(
            mars_dir,
            &target_root,
            target_name,
            outcomes,
            previous_managed_paths,
            force,
        ) {
            Ok(outcome) => {
                if !outcome.errors.is_empty() {
                    for err in &outcome.errors {
                        diag.warn(
                            "target-sync-error",
                            format!("target `{target_name}`: {err}"),
                        );
                    }
                }
                results.push(outcome);
            }
            Err(e) => {
                diag.warn(
                    "target-sync-failed",
                    format!("target `{target_name}` sync failed: {e}"),
                );
                results.push(TargetSyncOutcome {
                    target: target_name.clone(),
                    items_synced: 0,
                    items_removed: 0,
                    errors: vec![e.to_string()],
                });
            }
        }
    }

    results
}

/// Sync a single target directory from .mars/ canonical store.
fn sync_one_target(
    mars_dir: &Path,
    target_root: &Path,
    target_name: &str,
    outcomes: &[ActionOutcome],
    previous_managed_paths: &HashSet<PathBuf>,
    force: bool,
) -> Result<TargetSyncOutcome, MarsError> {
    let mut items_synced = 0;
    let mut items_removed = 0;
    let mut errors = Vec::new();

    // Ensure target directory exists
    std::fs::create_dir_all(target_root)?;

    // Track expected paths for orphan cleanup
    let mut expected_paths: HashSet<PathBuf> = HashSet::new();

    for outcome in outcomes {
        let dest_rel = outcome.dest_path.as_path();

        match &outcome.action {
            ActionTaken::Removed => {
                // Remove from target too
                let target_path = target_root.join(dest_rel);
                if target_path.exists() || target_path.symlink_metadata().is_ok() {
                    if let Err(e) = fs_ops::safe_remove(&target_path) {
                        errors.push(format!("failed to remove {}: {e}", dest_rel.display()));
                    } else {
                        items_removed += 1;
                    }
                }
            }
            ActionTaken::Skipped => {
                // Item is unchanged in .mars/ — still expected in target
                expected_paths.insert(dest_rel.to_path_buf());
                // Ensure it exists in target (idempotent convergence).
                // In --force mode, always refresh from .mars/ even if target exists.
                let source = mars_dir.join(dest_rel);
                let dest = target_root.join(dest_rel);
                if source.exists() && (force || !dest.exists()) {
                    match copy_item_to_target(&source, &dest) {
                        Ok(()) => items_synced += 1,
                        Err(e) => {
                            errors.push(format!("failed to copy {}: {e}", dest_rel.display()))
                        }
                    }
                }
            }
            _ => {
                // Installed, Updated, Merged, Conflicted, Kept
                // All of these mean content exists in .mars/ and should be copied to target
                expected_paths.insert(dest_rel.to_path_buf());
                let source = mars_dir.join(dest_rel);
                let dest = target_root.join(dest_rel);
                if source.exists() || source.symlink_metadata().is_ok() {
                    match copy_item_to_target(&source, &dest) {
                        Ok(()) => items_synced += 1,
                        Err(e) => {
                            errors.push(format!("failed to copy {}: {e}", dest_rel.display()))
                        }
                    }
                }
            }
        }
    }

    // Orphan cleanup: scan target for items not in expected set
    let orphan_removed = cleanup_orphans(
        target_root,
        &expected_paths,
        previous_managed_paths,
        &mut errors,
    );
    items_removed += orphan_removed;

    Ok(TargetSyncOutcome {
        target: target_name.to_string(),
        items_synced,
        items_removed,
        errors,
    })
}

/// Copy an item (file or directory) from .mars/ to a target directory.
///
/// Follows symlinks on the source side (D26 — targets get file copies, not symlinks).
/// Uses atomic operations via the reconcile layer.
fn copy_item_to_target(source: &Path, dest: &Path) -> Result<(), MarsError> {
    // Ensure parent directories exist
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Follow symlinks to determine if source is a file or directory
    let metadata = std::fs::metadata(source)?;

    if metadata.is_dir() {
        fs_ops::atomic_copy_dir(source, dest)?;
    } else if metadata.is_file() {
        fs_ops::atomic_copy_file(source, dest)?;
    }

    Ok(())
}

/// Clean up orphaned items in a target directory.
///
/// Scans `agents/` and `skills/` subdirectories in the target. Removes
/// entries only if they were previously managed by mars (present in old
/// lock ownership) and are no longer expected in the current sync.
/// Returns the number of items removed.
fn cleanup_orphans(
    target_root: &Path,
    expected: &HashSet<PathBuf>,
    previous_managed_paths: &HashSet<PathBuf>,
    errors: &mut Vec<String>,
) -> usize {
    let mut removed = 0;

    for subdir in ["agents", "skills"] {
        let scan_dir = target_root.join(subdir);
        if !scan_dir.exists() {
            continue;
        }

        // Skip if it's a symlink (legacy link setup — don't touch)
        if scan_dir.symlink_metadata().is_ok()
            && scan_dir
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        {
            continue;
        }

        let entries = match std::fs::read_dir(&scan_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Skip hidden files (like .mars-tmp-*)
            if name_str.starts_with('.') {
                continue;
            }

            let rel_path = PathBuf::from(subdir).join(&file_name);
            if previous_managed_paths.contains(&rel_path) && !expected.contains(&rel_path) {
                let full_path = entry.path();
                if let Err(e) = fs_ops::safe_remove(&full_path) {
                    errors.push(format!(
                        "failed to remove orphan {}: {e}",
                        rel_path.display()
                    ));
                } else {
                    removed += 1;
                }
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticCollector;
    use crate::sync::apply::{ActionOutcome, ActionTaken};
    use crate::types::{DestPath, ItemName};
    use tempfile::TempDir;

    fn make_outcome(dest: &str, action: ActionTaken) -> ActionOutcome {
        ActionOutcome {
            item_id: crate::lock::ItemId {
                kind: crate::lock::ItemKind::Agent,
                name: ItemName::from("test"),
            },
            action,
            dest_path: DestPath::from(dest),
            source_name: "test-source".into(),
            source_checksum: None,
            installed_checksum: None,
        }
    }

    fn managed_paths(paths: &[&str]) -> HashSet<PathBuf> {
        paths
            .iter()
            .map(|p| PathBuf::from(*p))
            .collect::<HashSet<PathBuf>>()
    }

    #[test]
    fn sync_copies_installed_items_to_target() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        // Set up .mars/ content
        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Coder").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Installed)];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&[]),
            false,
            &mut diag,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].items_synced, 1);
        assert!(results[0].errors.is_empty());
        assert!(target.join("agents/coder.md").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("agents/coder.md")).unwrap(),
            "# Coder"
        );
    }

    #[test]
    fn sync_removes_items_from_target() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(&mars_dir).unwrap();
        std::fs::create_dir_all(target.join("agents")).unwrap();
        std::fs::write(target.join("agents/old.md"), "# Old").unwrap();

        let outcomes = vec![make_outcome("agents/old.md", ActionTaken::Removed)];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&["agents/old.md"]),
            false,
            &mut diag,
        );

        assert_eq!(results[0].items_removed, 1);
        assert!(!target.join("agents/old.md").exists());
    }

    #[test]
    fn sync_cleans_up_previous_managed_orphans() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        // Set up .mars/ with one agent
        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Coder").unwrap();

        // Set up target with an extra agent (orphan)
        std::fs::create_dir_all(target.join("agents")).unwrap();
        std::fs::write(target.join("agents/orphan.md"), "# Orphan").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Installed)];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&["agents/orphan.md"]),
            false,
            &mut diag,
        );

        assert!(target.join("agents/coder.md").exists());
        assert!(!target.join("agents/orphan.md").exists());
        assert_eq!(results[0].items_removed, 1);
    }

    #[test]
    fn sync_preserves_unmanaged_files_in_target() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Coder").unwrap();

        std::fs::create_dir_all(target.join("agents")).unwrap();
        std::fs::write(target.join("agents/custom.md"), "# User custom").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Installed)];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&[]),
            false,
            &mut diag,
        );

        assert!(target.join("agents/coder.md").exists());
        assert!(target.join("agents/custom.md").exists());
        assert_eq!(results[0].items_removed, 0);
    }

    #[test]
    fn sync_multiple_targets() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Coder").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Installed)];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string(), ".claude".to_string()],
            &outcomes,
            &managed_paths(&[]),
            false,
            &mut diag,
        );

        assert_eq!(results.len(), 2);
        assert!(dir.path().join(".agents/agents/coder.md").exists());
        assert!(dir.path().join(".claude/agents/coder.md").exists());
    }

    #[test]
    fn sync_skill_directory() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("skills/planning")).unwrap();
        std::fs::write(mars_dir.join("skills/planning/SKILL.md"), "# Planning").unwrap();

        let mut outcome = make_outcome("skills/planning", ActionTaken::Installed);
        outcome.item_id.kind = crate::lock::ItemKind::Skill;
        let outcomes = vec![outcome];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&[]),
            false,
            &mut diag,
        );

        assert_eq!(results[0].items_synced, 1);
        assert!(target.join("skills/planning/SKILL.md").exists());
    }

    #[test]
    fn sync_convergence_on_rerun() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Coder").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Installed)];
        let mut diag = DiagnosticCollector::new();

        // First run
        sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&[]),
            false,
            &mut diag,
        );

        // Second run with Skipped action — should converge (file already exists)
        let outcomes2 = vec![make_outcome("agents/coder.md", ActionTaken::Skipped)];
        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes2,
            &managed_paths(&["agents/coder.md"]),
            false,
            &mut diag,
        );

        assert!(target.join("agents/coder.md").exists());
        // items_synced should be 0 since file already exists
        assert_eq!(results[0].items_synced, 0);
    }

    #[test]
    fn sync_force_refreshes_skipped_target_content() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Canonical").unwrap();

        std::fs::create_dir_all(target.join("agents")).unwrap();
        std::fs::write(target.join("agents/coder.md"), "# Tampered").unwrap();

        let outcomes = vec![make_outcome("agents/coder.md", ActionTaken::Skipped)];
        let mut diag = DiagnosticCollector::new();
        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&["agents/coder.md"]),
            true,
            &mut diag,
        );

        assert_eq!(results[0].items_synced, 1);
        assert_eq!(
            std::fs::read_to_string(target.join("agents/coder.md")).unwrap(),
            "# Canonical"
        );
    }
}
