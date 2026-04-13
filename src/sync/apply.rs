use std::path::Path;

use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind};
use crate::reconcile::fs_ops;
use crate::sync::plan::{PlannedAction, SyncPlan};
use crate::sync::target::TargetItem;
pub use crate::sync::types::SyncOptions;
use crate::types::{ContentHash, DestPath, ItemName, SourceName};

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
    pub dest_path: DestPath,
    /// Which source this item came from.
    pub source_name: SourceName,
    /// Source checksum (pre-rewrite hash of source content).
    pub source_checksum: Option<ContentHash>,
    /// Installed checksum (post-rewrite hash of what was written to disk).
    pub installed_checksum: Option<ContentHash>,
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

/// Execute the sync plan, applying changes to disk.
///
/// For each action:
/// - Install: copy source content to dest (atomic_write or atomic_install_dir)
/// - Overwrite: replace existing with new source content
/// - Merge: three-way merge using base from cache
/// - Remove: delete file/dir from disk
/// - Skip/KeepLocal: record as no-op
///
/// Returns outcomes with both source_checksum and installed_checksum.
/// The installed_checksum may differ from source_checksum when frontmatter
/// rewriting occurred.
pub fn execute(
    root: &Path,
    plan: &SyncPlan,
    options: &SyncOptions,
    cache_bases_dir: &Path,
) -> Result<ApplyResult, MarsError> {
    let mut outcomes = Vec::new();

    for action in &plan.actions {
        let outcome = if options.dry_run {
            // Dry run: compute the outcome without touching disk
            dry_run_action(action)
        } else {
            execute_action(root, action, cache_bases_dir)?
        };
        outcomes.push(outcome);
    }

    Ok(ApplyResult { outcomes })
}

/// Execute a single action, writing to disk.
fn execute_action(
    root: &Path,
    action: &PlannedAction,
    cache_bases_dir: &Path,
) -> Result<ActionOutcome, MarsError> {
    match action {
        PlannedAction::Install { target } => {
            let dest = root.join(&target.dest_path);

            // Read source content and install
            let installed_checksum = install_item(target, &dest)?;

            // Cache the installed content as base for future merges
            cache_base_content(cache_bases_dir, &installed_checksum, &dest, target.id.kind)?;

            Ok(ActionOutcome {
                item_id: target.id.clone(),
                action: ActionTaken::Installed,
                dest_path: target.dest_path.clone(),
                source_name: target.source_name.clone(),
                source_checksum: Some(target.source_hash.clone()),
                installed_checksum: Some(installed_checksum),
            })
        }

        PlannedAction::Overwrite { target } => {
            let dest = root.join(&target.dest_path);

            // Install (overwrite) source content
            let installed_checksum = install_item(target, &dest)?;

            // Update base cache
            cache_base_content(cache_bases_dir, &installed_checksum, &dest, target.id.kind)?;

            Ok(ActionOutcome {
                item_id: target.id.clone(),
                action: ActionTaken::Updated,
                dest_path: target.dest_path.clone(),
                source_name: target.source_name.clone(),
                source_checksum: Some(target.source_hash.clone()),
                installed_checksum: Some(installed_checksum),
            })
        }

        PlannedAction::Merge {
            target,
            base_content,
            local_path,
        } => {
            let dest = root.join(&target.dest_path);
            let full_local_path = root.join(local_path);

            // Read source (theirs) content
            let theirs_content = read_target_content_for_merge(target)?;

            // Read local content
            let local_content = read_item_content(&full_local_path, target.id.kind)?;

            // Perform three-way merge
            let labels = crate::merge::MergeLabels {
                base: "base (last sync)".into(),
                local: "local".into(),
                theirs: format!("{}@{}", target.source_name, "upstream"),
            };

            let merge_result = crate::merge::merge_content(
                base_content,
                &local_content,
                &theirs_content,
                &labels,
            )?;

            // Write merged content
            fs_ops::atomic_write_file(&dest, &merge_result.content)?;

            let installed_checksum =
                ContentHash::from(crate::hash::hash_bytes(&merge_result.content));

            // Cache the merged content as new base
            cache_base_content(cache_bases_dir, &installed_checksum, &dest, target.id.kind)?;

            let action_taken = if merge_result.has_conflicts {
                ActionTaken::Conflicted
            } else {
                ActionTaken::Merged
            };

            Ok(ActionOutcome {
                item_id: target.id.clone(),
                action: action_taken,
                dest_path: target.dest_path.clone(),
                source_name: target.source_name.clone(),
                source_checksum: Some(target.source_hash.clone()),
                installed_checksum: Some(installed_checksum),
            })
        }

        PlannedAction::Remove { locked } => {
            let dest = root.join(&locked.dest_path);
            if dest.exists() {
                fs_ops::safe_remove(&dest)?;
            }

            let item_id = ItemId {
                kind: locked.kind,
                name: ItemName::from(extract_name_from_dest(&locked.dest_path, locked.kind)),
            };

            Ok(ActionOutcome {
                item_id,
                action: ActionTaken::Removed,
                dest_path: locked.dest_path.clone(),
                source_name: locked.source.clone(),
                source_checksum: None,
                installed_checksum: None,
            })
        }

        PlannedAction::Skip {
            item_id,
            dest_path,
            source_name,
            reason: _,
        } => Ok(ActionOutcome {
            item_id: item_id.clone(),
            action: ActionTaken::Skipped,
            dest_path: dest_path.clone(),
            source_name: source_name.clone(),
            source_checksum: None,
            installed_checksum: None,
        }),

        PlannedAction::KeepLocal {
            item_id,
            dest_path,
            source_name,
        } => Ok(ActionOutcome {
            item_id: item_id.clone(),
            action: ActionTaken::Kept,
            dest_path: dest_path.clone(),
            source_name: source_name.clone(),
            source_checksum: None,
            installed_checksum: None,
        }),
    }
}

/// Produce a dry-run outcome without touching disk.
fn dry_run_action(action: &PlannedAction) -> ActionOutcome {
    match action {
        PlannedAction::Install { target } => ActionOutcome {
            item_id: target.id.clone(),
            action: ActionTaken::Installed,
            dest_path: target.dest_path.clone(),
            source_name: target.source_name.clone(),
            source_checksum: Some(target.source_hash.clone()),
            installed_checksum: None, // Can't know without actually installing
        },
        PlannedAction::Overwrite { target } => ActionOutcome {
            item_id: target.id.clone(),
            action: ActionTaken::Updated,
            dest_path: target.dest_path.clone(),
            source_name: target.source_name.clone(),
            source_checksum: Some(target.source_hash.clone()),
            installed_checksum: None,
        },
        PlannedAction::Merge { target, .. } => ActionOutcome {
            item_id: target.id.clone(),
            action: ActionTaken::Merged,
            dest_path: target.dest_path.clone(),
            source_name: target.source_name.clone(),
            source_checksum: Some(target.source_hash.clone()),
            installed_checksum: None,
        },
        PlannedAction::Remove { locked } => {
            let item_id = ItemId {
                kind: locked.kind,
                name: ItemName::from(extract_name_from_dest(&locked.dest_path, locked.kind)),
            };
            ActionOutcome {
                item_id,
                action: ActionTaken::Removed,
                dest_path: locked.dest_path.clone(),
                source_name: locked.source.clone(),
                source_checksum: None,
                installed_checksum: None,
            }
        }
        PlannedAction::Skip {
            item_id,
            dest_path,
            source_name,
            ..
        } => ActionOutcome {
            item_id: item_id.clone(),
            action: ActionTaken::Skipped,
            dest_path: dest_path.clone(),
            source_name: source_name.clone(),
            source_checksum: None,
            installed_checksum: None,
        },
        PlannedAction::KeepLocal {
            item_id,
            dest_path,
            source_name,
        } => ActionOutcome {
            item_id: item_id.clone(),
            action: ActionTaken::Kept,
            dest_path: dest_path.clone(),
            source_name: source_name.clone(),
            source_checksum: None,
            installed_checksum: None,
        },
    }
}

/// Install an item (file or directory) to the destination.
///
/// Returns the installed checksum (hash of what was written to disk).
fn install_item(target: &TargetItem, dest: &Path) -> Result<ContentHash, MarsError> {
    match target.id.kind {
        ItemKind::Agent => {
            let content = content_to_install(target)?;
            fs_ops::atomic_write_file(dest, &content)?;
            Ok(ContentHash::from(crate::hash::hash_bytes(&content)))
        }
        ItemKind::Skill => {
            if target.is_flat_skill {
                crate::fs::atomic_install_dir_filtered(
                    &target.source_path,
                    dest,
                    crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL,
                )?;
            } else {
                fs_ops::atomic_install_dir(&target.source_path, dest)?;
            }
            crate::hash::compute_hash(dest, ItemKind::Skill).map(ContentHash::from)
        }
    }
}

/// Read bytes to install for an agent, honoring in-memory rewrite overrides.
fn content_to_install(target: &TargetItem) -> Result<Vec<u8>, MarsError> {
    if let Some(content) = &target.rewritten_content {
        Ok(content.as_bytes().to_vec())
    } else {
        Ok(std::fs::read(&target.source_path)?)
    }
}

/// Read source content for merge operations.
fn read_target_content_for_merge(target: &TargetItem) -> Result<Vec<u8>, MarsError> {
    match target.id.kind {
        ItemKind::Agent => content_to_install(target),
        ItemKind::Skill => read_item_content(&target.source_path, target.id.kind),
    }
}

/// Read content from an item (file for agents, concatenated for skills).
/// For merge purposes, we only support file-level merge (agents).
/// Skills that need merging would require per-file merge, which is complex.
/// For now, read the primary file content.
fn read_item_content(path: &Path, kind: ItemKind) -> Result<Vec<u8>, MarsError> {
    match kind {
        ItemKind::Agent => Ok(std::fs::read(path)?),
        ItemKind::Skill => {
            // For skills (directories), read the SKILL.md as the merge target
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                Ok(std::fs::read(&skill_md)?)
            } else {
                Ok(Vec::new())
            }
        }
    }
}

/// Cache base content for future three-way merges.
///
/// Content-addressed by installed checksum. Written after every install/overwrite.
/// Missing cache = degrade to two-way diff (more conflict markers), not crash.
fn cache_base_content(
    cache_bases_dir: &Path,
    installed_checksum: &ContentHash,
    dest: &Path,
    kind: ItemKind,
) -> Result<(), MarsError> {
    std::fs::create_dir_all(cache_bases_dir)?;
    let cache_path = cache_bases_dir.join(installed_checksum.as_ref());

    // Only cache if not already present (content-addressed = immutable)
    if cache_path.exists() {
        return Ok(());
    }

    match kind {
        ItemKind::Agent => {
            let content = std::fs::read(dest)?;
            fs_ops::atomic_write_file(&cache_path, &content)?;
        }
        ItemKind::Skill => {
            // For skills, cache the SKILL.md content (the merge-relevant part)
            let skill_md = dest.join("SKILL.md");
            if skill_md.exists() {
                let content = std::fs::read(&skill_md)?;
                fs_ops::atomic_write_file(&cache_path, &content)?;
            }
        }
    }

    Ok(())
}

/// Extract the item name from a destination path.
fn extract_name_from_dest(dest_path: &DestPath, kind: ItemKind) -> String {
    let path = dest_path.as_path();
    match kind {
        ItemKind::Agent => path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        ItemKind::Skill => path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

/// Prune orphans: items in old lock but not in new target.
///
/// This is handled by the Remove action in the plan, but exposed
/// separately for the sync pipeline if needed.
pub fn prune_orphans(
    root: &Path,
    lock: &crate::lock::LockFile,
    target: &crate::sync::target::TargetState,
) -> Result<Vec<ActionOutcome>, MarsError> {
    let mut outcomes = Vec::new();

    for (dest_path_str, locked_item) in &lock.items {
        if !target.items.contains_key(dest_path_str) {
            let dest = root.join(dest_path_str);
            if dest.exists() {
                fs_ops::safe_remove(&dest)?;
            }
            outcomes.push(ActionOutcome {
                item_id: ItemId {
                    kind: locked_item.kind,
                    name: ItemName::from(extract_name_from_dest(dest_path_str, locked_item.kind)),
                },
                action: ActionTaken::Removed,
                dest_path: dest_path_str.clone(),
                source_name: locked_item.source.clone(),
                source_checksum: None,
                installed_checksum: None,
            });
        }
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash;
    use crate::lock::{ItemId, ItemKind, LockedItem};
    use crate::sync::plan::{PlannedAction, SyncPlan};
    use crate::sync::target::TargetItem;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_agent_target(name: &str, source_path: PathBuf, content: &[u8]) -> TargetItem {
        TargetItem {
            id: ItemId {
                kind: ItemKind::Agent,
                name: name.into(),
            },
            source_name: "test-source".into(),
            origin: crate::types::SourceOrigin::Dependency("test-source".into()),
            source_id: crate::types::SourceId::Path {
                canonical: source_path.clone(),
            },
            source_path,
            dest_path: format!("agents/{name}.md").into(),
            source_hash: hash::hash_bytes(content).into(),
            is_flat_skill: false,
            rewritten_content: None,
        }
    }

    fn setup_source_agent(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let agents_dir = dir.join("source").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        let path = agents_dir.join(format!("{name}.md"));
        fs::write(&path, content).unwrap();
        path
    }

    // === Install tests ===

    #[test]
    fn install_creates_new_file() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let content = b"# new agent content";
        let source_path = setup_source_agent(source_dir.path(), "coder", content);
        let target = make_agent_target("coder", source_path, content);

        let plan = SyncPlan {
            actions: vec![PlannedAction::Install {
                target: target.clone(),
            }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert_eq!(result.outcomes.len(), 1);

        let outcome = &result.outcomes[0];
        assert!(matches!(outcome.action, ActionTaken::Installed));

        // Verify file was created
        let installed_path = root.path().join("agents/coder.md");
        assert!(installed_path.exists());
        assert_eq!(fs::read(&installed_path).unwrap(), content);

        // Verify checksums
        assert_eq!(
            outcome.source_checksum.as_deref(),
            Some(hash::hash_bytes(content).as_str())
        );
        assert!(outcome.installed_checksum.is_some());
    }

    #[test]
    fn install_caches_base_content() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let content = b"# cached content";
        let source_path = setup_source_agent(source_dir.path(), "coder", content);
        let target = make_agent_target("coder", source_path, content);

        let plan = SyncPlan {
            actions: vec![PlannedAction::Install { target }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        let installed_checksum = result.outcomes[0].installed_checksum.as_ref().unwrap();

        // Verify base content was cached
        let cached = bases_dir.join(installed_checksum.as_ref());
        assert!(cached.exists(), "base content should be cached");
        assert_eq!(fs::read(&cached).unwrap(), content);
    }

    // === Overwrite tests ===

    #[test]
    fn overwrite_replaces_existing_file() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        // Create existing file
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), b"# old content").unwrap();

        let new_content = b"# new content";
        let source_path = setup_source_agent(source_dir.path(), "coder", new_content);
        let target = make_agent_target("coder", source_path, new_content);

        let plan = SyncPlan {
            actions: vec![PlannedAction::Overwrite { target }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Updated));

        let installed = fs::read(root.path().join("agents/coder.md")).unwrap();
        assert_eq!(installed, new_content);
    }

    // === Remove tests ===

    #[test]
    fn remove_deletes_file() {
        let root = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        // Create file to remove
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("orphan.md"), b"# orphan").unwrap();

        let locked = LockedItem {
            source: "old-source".into(),
            kind: ItemKind::Agent,
            version: None,
            source_checksum: "sha256:aaa".into(),
            installed_checksum: "sha256:bbb".into(),
            dest_path: "agents/orphan.md".into(),
        };

        let plan = SyncPlan {
            actions: vec![PlannedAction::Remove { locked }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Removed));
        assert!(!root.path().join("agents/orphan.md").exists());
    }

    #[test]
    fn remove_skill_directory() {
        let root = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        // Create skill directory
        let skill_dir = root.path().join("skills/old-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), b"# old skill").unwrap();

        let locked = LockedItem {
            source: "old-source".into(),
            kind: ItemKind::Skill,
            version: None,
            source_checksum: "sha256:aaa".into(),
            installed_checksum: "sha256:bbb".into(),
            dest_path: "skills/old-skill".into(),
        };

        let plan = SyncPlan {
            actions: vec![PlannedAction::Remove { locked }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Removed));
        assert!(!root.path().join("skills/old-skill").exists());
    }

    // === Dry run tests ===

    #[test]
    fn dry_run_does_not_modify_files() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let content = b"# new agent";
        let source_path = setup_source_agent(source_dir.path(), "coder", content);
        let target = make_agent_target("coder", source_path, content);

        let plan = SyncPlan {
            actions: vec![PlannedAction::Install { target }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: true,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert_eq!(result.outcomes.len(), 1);
        assert!(matches!(result.outcomes[0].action, ActionTaken::Installed));

        // File should NOT exist
        assert!(!root.path().join("agents/coder.md").exists());
    }

    // === Skip/KeepLocal tests ===

    #[test]
    fn skip_produces_skipped_outcome() {
        let root = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let plan = SyncPlan {
            actions: vec![PlannedAction::Skip {
                item_id: ItemId {
                    kind: ItemKind::Agent,
                    name: "stable".into(),
                },
                dest_path: "agents/stable.md".into(),
                source_name: "base".into(),
                reason: "unchanged",
            }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Skipped));
        assert_eq!(
            result.outcomes[0].dest_path,
            crate::types::DestPath::from("agents/stable.md")
        );
        assert_eq!(result.outcomes[0].source_name, "base");
    }

    #[test]
    fn keep_local_produces_kept_outcome() {
        let root = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let plan = SyncPlan {
            actions: vec![PlannedAction::KeepLocal {
                item_id: ItemId {
                    kind: ItemKind::Agent,
                    name: "modified".into(),
                },
                dest_path: "agents/modified.md".into(),
                source_name: "base".into(),
            }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Kept));
        assert_eq!(
            result.outcomes[0].dest_path,
            crate::types::DestPath::from("agents/modified.md")
        );
        assert_eq!(result.outcomes[0].source_name, "base");
    }

    // === Install skill directory tests ===

    #[test]
    fn install_skill_directory() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        // Create source skill directory
        let source_skill = source_dir.path().join("skills/planning");
        fs::create_dir_all(&source_skill).unwrap();
        fs::write(source_skill.join("SKILL.md"), b"# Planning skill").unwrap();
        fs::write(source_skill.join("helper.md"), b"# Helper").unwrap();

        let skill_hash = hash::compute_hash(&source_skill, ItemKind::Skill).unwrap();

        let target = TargetItem {
            id: ItemId {
                kind: ItemKind::Skill,
                name: "planning".into(),
            },
            source_name: "test".into(),
            origin: crate::types::SourceOrigin::Dependency("test".into()),
            source_id: crate::types::SourceId::Path {
                canonical: source_skill.clone(),
            },
            source_path: source_skill,
            dest_path: "skills/planning".into(),
            source_hash: skill_hash.into(),
            is_flat_skill: false,
            rewritten_content: None,
        };

        let plan = SyncPlan {
            actions: vec![PlannedAction::Install { target }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        let result = execute(root.path(), &plan, &options, &bases_dir).unwrap();
        assert!(matches!(result.outcomes[0].action, ActionTaken::Installed));

        let installed_dir = root.path().join("skills/planning");
        assert!(installed_dir.exists());
        assert!(installed_dir.join("SKILL.md").exists());
        assert!(installed_dir.join("helper.md").exists());
        assert_eq!(
            fs::read_to_string(installed_dir.join("SKILL.md")).unwrap(),
            "# Planning skill"
        );
    }

    #[test]
    fn install_flat_skill_excludes_repo_metadata() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let bases_dir = cache_dir.path().join("bases");

        let flat_source = source_dir.path().join("flat-skill");
        fs::create_dir_all(flat_source.join(".git")).unwrap();
        fs::create_dir_all(flat_source.join("resources")).unwrap();
        fs::write(flat_source.join("SKILL.md"), b"# Flat skill").unwrap();
        fs::write(flat_source.join("resources/guide.md"), b"# Guide").unwrap();
        fs::write(flat_source.join("mars.toml"), b"[sources]").unwrap();
        fs::write(flat_source.join(".gitignore"), b"target/").unwrap();
        fs::write(flat_source.join(".git/config"), b"[core]").unwrap();

        let source_hash = hash::compute_skill_hash_filtered(
            &flat_source,
            crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL,
        )
        .unwrap();

        let target = TargetItem {
            id: ItemId {
                kind: ItemKind::Skill,
                name: "flat-skill".into(),
            },
            source_name: "test".into(),
            origin: crate::types::SourceOrigin::Dependency("test".into()),
            source_id: crate::types::SourceId::Path {
                canonical: flat_source.clone(),
            },
            source_path: flat_source,
            dest_path: "skills/flat-skill".into(),
            source_hash: source_hash.into(),
            is_flat_skill: true,
            rewritten_content: None,
        };

        let plan = SyncPlan {
            actions: vec![PlannedAction::Install { target }],
        };

        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };

        execute(root.path(), &plan, &options, &bases_dir).unwrap();

        let installed = root.path().join("skills/flat-skill");
        assert!(installed.join("SKILL.md").exists());
        assert!(installed.join("resources/guide.md").exists());
        assert!(!installed.join(".git").exists());
        assert!(!installed.join("mars.toml").exists());
        assert!(!installed.join(".gitignore").exists());
    }

    // === Prune orphans tests ===

    #[test]
    fn prune_removes_orphaned_items() {
        let root = TempDir::new().unwrap();

        // Create orphaned file
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("old.md"), b"# orphan").unwrap();

        let mut lock_items = indexmap::IndexMap::new();
        lock_items.insert(
            "agents/old.md".into(),
            LockedItem {
                source: "old-source".into(),
                kind: ItemKind::Agent,
                version: None,
                source_checksum: "sha256:aaa".into(),
                installed_checksum: "sha256:bbb".into(),
                dest_path: "agents/old.md".into(),
            },
        );
        let lock = crate::lock::LockFile {
            version: 1,
            dependencies: indexmap::IndexMap::new(),
            items: lock_items,
        };

        // Empty target = orphan should be pruned
        let target = crate::sync::target::TargetState {
            items: indexmap::IndexMap::new(),
        };

        let outcomes = prune_orphans(root.path(), &lock, &target).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].action, ActionTaken::Removed));
        assert!(!root.path().join("agents/old.md").exists());
    }

    // === extract_name_from_dest tests ===

    #[test]
    fn extract_agent_name() {
        assert_eq!(
            extract_name_from_dest(
                &crate::types::DestPath::from("agents/coder.md"),
                ItemKind::Agent
            ),
            "coder"
        );
    }

    #[test]
    fn extract_skill_name() {
        assert_eq!(
            extract_name_from_dest(
                &crate::types::DestPath::from("skills/planning"),
                ItemKind::Skill
            ),
            "planning"
        );
    }
}
