//! Target sync — copy content from .mars/ canonical store to managed targets.
//!
//! After `apply_plan()` writes resolved content to `.mars/agents/` and `.mars/skills/`,
//! this module copies that content to all configured native target directories (`.claude/`, etc.).
//!
//! All targets are managed outputs — they get copies (not symlinks) of .mars/ content.

use std::collections::HashSet;
use std::path::Path;

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::reconcile::fs_ops;
use crate::sync::apply::{ActionOutcome, ActionTaken};
use crate::types::ContentHash;

/// A directory that mars manages — materialized from .mars/.
#[derive(Debug, Clone)]
pub struct ManagedTarget {
    /// Target directory path relative to project root (e.g. ".claude").
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
    previous_managed_paths: &HashSet<String>,
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
            diag,
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
    previous_managed_paths: &HashSet<String>,
    force: bool,
    diag: &mut DiagnosticCollector,
) -> Result<TargetSyncOutcome, MarsError> {
    let mut items_synced = 0;
    let mut items_removed = 0;
    let mut errors = Vec::new();

    // Ensure target directory exists
    std::fs::create_dir_all(target_root)?;

    // Track expected paths for orphan cleanup
    let mut expected_paths: HashSet<String> = HashSet::new();
    let native_skill_variant_key = crate::target::TargetRegistry::new()
        .get(target_name)
        .and_then(|adapter| adapter.skill_variant_key())
        .map(str::to_owned);

    for outcome in outcomes {
        if outcome.item_id.kind == crate::lock::ItemKind::BootstrapDoc {
            // Package-level bootstrap docs are Meridian-only canonical content.
            // Skill-level bootstrap docs still reach native targets as ordinary
            // files inside skill directories.
            continue;
        }
        let dest_rel = outcome.dest_path.as_str();

        match &outcome.action {
            ActionTaken::Removed => {
                // Remove from target too
                let target_path = target_root.join(dest_rel);
                if target_path.exists() || target_path.symlink_metadata().is_ok() {
                    if let Err(e) = fs_ops::safe_remove(&target_path) {
                        errors.push(format!("failed to remove {dest_rel}: {e}"));
                    } else {
                        items_removed += 1;
                    }
                }
            }
            ActionTaken::Skipped => {
                // Item is unchanged in .mars/ — still expected in target
                expected_paths.insert(dest_rel.to_string());
                let source = mars_dir.join(dest_rel);
                let dest = target_root.join(dest_rel);
                if source.exists() || source.symlink_metadata().is_ok() {
                    if force || !dest.exists() {
                        match copy_item_to_target(
                            &source,
                            &dest,
                            outcome.item_id.kind,
                            outcome.item_id.name.as_str(),
                            native_skill_variant_key.as_deref(),
                            diag,
                        ) {
                            Ok(()) => items_synced += 1,
                            Err(e) => errors.push(format!("failed to copy {dest_rel}: {e}")),
                        }
                    } else if native_skill_variant_key.is_none()
                        && let Some(expected_checksum) = &outcome.installed_checksum
                    {
                        match crate::hash::compute_hash(&dest, outcome.item_id.kind) {
                            Ok(actual) => {
                                let actual = ContentHash::from(actual);
                                if &actual != expected_checksum {
                                    diag.warn(
                                        "target-divergent",
                                        format!(
                                            "target `{target_name}` item `{}` diverged from `.mars` (preserved local content; run `mars sync --force` or `mars repair` to reset)",
                                            dest_rel
                                        ),
                                    );
                                }
                            }
                            Err(e) => {
                                errors.push(format!("failed to verify {dest_rel} checksum: {e}"))
                            }
                        }
                    }
                }
            }
            _ => {
                // Installed, Updated, Merged, Conflicted, Kept
                // All of these mean content exists in .mars/ and should be copied to target
                expected_paths.insert(dest_rel.to_string());
                let source = mars_dir.join(dest_rel);
                let dest = target_root.join(dest_rel);
                if source.exists() || source.symlink_metadata().is_ok() {
                    match copy_item_to_target(
                        &source,
                        &dest,
                        outcome.item_id.kind,
                        outcome.item_id.name.as_str(),
                        native_skill_variant_key.as_deref(),
                        diag,
                    ) {
                        Ok(()) => items_synced += 1,
                        Err(e) => errors.push(format!("failed to copy {dest_rel}: {e}")),
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
fn copy_item_to_target(
    source: &Path,
    dest: &Path,
    kind: crate::lock::ItemKind,
    item_name: &str,
    native_skill_variant_key: Option<&str>,
    diag: &mut DiagnosticCollector,
) -> Result<(), MarsError> {
    if kind == crate::lock::ItemKind::Skill && native_skill_variant_key.is_some() {
        crate::compiler::variants::validate_skill_variants(source, item_name, diag);
        return crate::compiler::variants::project_skill_for_target(
            source,
            dest,
            native_skill_variant_key,
            diag,
            item_name,
        );
    }

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
/// Uses lock v2 output records (via `previous_managed_paths`) to determine
/// what was managed in the prior sync, rather than scanning hardcoded
/// subdirectories. Removes entries that were previously managed but are no
/// longer expected in the current sync.
///
/// Returns the number of items removed.
fn cleanup_orphans(
    target_root: &Path,
    expected: &HashSet<String>,
    previous_managed_paths: &HashSet<String>,
    errors: &mut Vec<String>,
) -> usize {
    let mut removed = 0;

    // Lock-driven: iterate paths from the old lock, not hardcoded subdirectories.
    // Only remove entries that were previously managed and are no longer expected.
    for managed_path in previous_managed_paths {
        if expected.contains(managed_path) {
            continue;
        }

        let full_path = target_root.join(managed_path);

        // Skip if the path doesn't exist (already removed or never synced to this target).
        if !full_path.exists() && full_path.symlink_metadata().is_err() {
            continue;
        }

        // Skip symlinked paths (legacy link setup — don't touch).
        if full_path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }

        if let Err(e) = fs_ops::safe_remove(&full_path) {
            errors.push(format!("failed to remove orphan {managed_path}: {e}"));
        } else {
            removed += 1;
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticCollector;
    use crate::hash;
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

    fn managed_paths(paths: &[&str]) -> HashSet<String> {
        paths
            .iter()
            .map(|p| (*p).to_string())
            .collect::<HashSet<String>>()
    }

    fn make_skipped_with_checksum(dest: &str, checksum: &str) -> ActionOutcome {
        let mut outcome = make_outcome(dest, ActionTaken::Skipped);
        outcome.installed_checksum = Some(checksum.into());
        outcome
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
    fn sync_projects_skills_for_native_harness_targets() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".claude");

        std::fs::create_dir_all(mars_dir.join("skills/planning/resources")).unwrap();
        std::fs::create_dir_all(mars_dir.join("skills/planning/variants/claude")).unwrap();
        std::fs::create_dir_all(target.join("skills")).unwrap();
        std::fs::write(target.join("skills/orphan"), "# Orphan").unwrap();
        std::fs::write(mars_dir.join("skills/planning/SKILL.md"), "# Base").unwrap();
        std::fs::write(
            mars_dir.join("skills/planning/resources/BOOTSTRAP.md"),
            "# Bootstrap",
        )
        .unwrap();
        std::fs::write(
            mars_dir.join("skills/planning/variants/claude/SKILL.md"),
            "# Claude",
        )
        .unwrap();

        let mut outcome = make_outcome("skills/planning", ActionTaken::Installed);
        outcome.item_id.kind = crate::lock::ItemKind::Skill;
        let outcomes = vec![outcome];
        let mut diag = DiagnosticCollector::new();

        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".claude".to_string()],
            &outcomes,
            &managed_paths(&["skills/planning", "skills/orphan"]),
            false,
            &mut diag,
        );

        assert_eq!(results[0].items_synced, 1);
        assert_eq!(
            std::fs::read_to_string(target.join("skills/planning/SKILL.md")).unwrap(),
            "# Claude"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("skills/planning/resources/BOOTSTRAP.md")).unwrap(),
            "# Bootstrap"
        );
        assert!(!target.join("skills/planning/variants").exists());
        assert!(!target.join("skills/orphan").exists());
    }

    #[test]
    fn cleanup_orphans_uses_forward_slash_keys_for_expected_paths() {
        let dir = TempDir::new().unwrap();
        let target_root = dir.path().join(".agents");
        std::fs::create_dir_all(target_root.join("agents")).unwrap();
        std::fs::write(target_root.join("agents/coder.md"), "# Managed").unwrap();
        std::fs::write(target_root.join("agents/orphan.md"), "# Orphan").unwrap();

        let mut expected = HashSet::new();
        expected.insert(
            DestPath::new(r"agents\coder.md")
                .unwrap()
                .as_str()
                .to_string(),
        );

        let removed = cleanup_orphans(
            &target_root,
            &expected,
            &managed_paths(&["agents/coder.md", "agents/orphan.md"]),
            &mut Vec::new(),
        );

        assert_eq!(removed, 1);
        assert!(target_root.join("agents/coder.md").exists());
        assert!(!target_root.join("agents/orphan.md").exists());
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

    #[test]
    fn sync_skipped_recopies_missing_target() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Canonical").unwrap();

        let checksum = hash::hash_bytes(b"# Canonical");
        let outcomes = vec![make_skipped_with_checksum("agents/coder.md", &checksum)];
        let mut diag = DiagnosticCollector::new();
        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&["agents/coder.md"]),
            false,
            &mut diag,
        );

        assert_eq!(results[0].items_synced, 1);
        assert!(target.join("agents/coder.md").exists());
    }

    #[test]
    fn sync_skipped_warns_on_divergent_target_and_preserves_local_content() {
        let dir = TempDir::new().unwrap();
        let mars_dir = dir.path().join(".mars");
        let target = dir.path().join(".agents");

        std::fs::create_dir_all(mars_dir.join("agents")).unwrap();
        std::fs::write(mars_dir.join("agents/coder.md"), "# Canonical").unwrap();

        std::fs::create_dir_all(target.join("agents")).unwrap();
        std::fs::write(target.join("agents/coder.md"), "# Locally edited").unwrap();

        let checksum = hash::hash_bytes(b"# Canonical");
        let outcomes = vec![make_skipped_with_checksum("agents/coder.md", &checksum)];
        let mut diag = DiagnosticCollector::new();
        let results = sync_managed_targets(
            dir.path(),
            &mars_dir,
            &[".agents".to_string()],
            &outcomes,
            &managed_paths(&["agents/coder.md"]),
            false,
            &mut diag,
        );

        assert_eq!(results[0].items_synced, 0);
        assert_eq!(
            std::fs::read_to_string(target.join("agents/coder.md")).unwrap(),
            "# Locally edited"
        );

        let diagnostics = diag.drain();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "target-divergent" && d.message.contains("agents/coder.md"))
        );
    }
}
