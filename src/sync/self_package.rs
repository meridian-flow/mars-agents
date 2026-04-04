//! Local package (`[package]` / `_self`) lifecycle management.
//!
//! Handles discovery of local agents and skills, symlink injection into the sync plan,
//! orphan pruning for stale `_self` entries, and lock item construction.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use crate::error::MarsError;
use crate::hash;
use crate::lock::ItemKind;
use crate::sync::plan;
use crate::types::{ContentHash, DestPath, ItemName};

/// A local package item discovered under the project root.
pub(crate) struct LocalItem {
    pub kind: ItemKind,
    pub name: ItemName,
    /// Absolute path to source — for agents, the .md file; for skills, the directory.
    pub source_path: PathBuf,
    /// Relative destination under managed root.
    pub dest_rel: DestPath,
}

/// Discover local package items (agents and skills) at the project root.
///
/// Called when `[package]` is present in `mars.toml`. Scans:
/// - `project_root/agents/*.md` → agent items
/// - `project_root/skills/*/` (directories containing SKILL.md) → skill items
pub(crate) fn discover_local_items(project_root: &Path) -> Result<Vec<LocalItem>, MarsError> {
    let mut items = Vec::new();

    // Discover agents
    let agents_dir = project_root.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") && path.is_file() {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                items.push(LocalItem {
                    kind: ItemKind::Agent,
                    name: ItemName::from(name.as_str()),
                    source_path: path.canonicalize().unwrap_or(path.clone()),
                    dest_rel: format!("agents/{}.md", name).into(),
                });
            }
        }
    }

    // Discover skills
    let skills_dir = project_root.join("skills");
    if skills_dir.is_dir() {
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").exists() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                items.push(LocalItem {
                    kind: ItemKind::Skill,
                    name: ItemName::from(name.as_str()),
                    source_path: path.canonicalize().unwrap_or(path.clone()),
                    dest_rel: format!("skills/{}", name).into(),
                });
            }
        }
    }

    Ok(items)
}

/// Build lock items from discovered local items.
pub(crate) fn build_self_lock_items(
    items: &[LocalItem],
) -> Result<Vec<crate::lock::SelfLockItem>, MarsError> {
    let mut lock_items = Vec::with_capacity(items.len());
    for item in items {
        let source_checksum = ContentHash::from(hash::compute_hash(&item.source_path, item.kind)?);
        lock_items.push(crate::lock::SelfLockItem {
            dest_path: item.dest_rel.clone(),
            kind: item.kind,
            source_checksum,
        });
    }
    Ok(lock_items)
}

/// Inject local package symlinks into the sync plan and handle collisions.
///
/// Returns the set of skipped `_self` destinations (for unmanaged collision avoidance).
pub(crate) fn inject_self_items(
    config_has_package: bool,
    project_root: &Path,
    managed_root: &Path,
    old_lock: &crate::lock::LockFile,
    target_state: &mut crate::sync::target::TargetState,
    sync_plan: &mut plan::SyncPlan,
) -> Result<HashSet<DestPath>, MarsError> {
    let mut skipped_self_dests: HashSet<DestPath> = HashSet::new();

    if config_has_package {
        let self_items = discover_local_items(project_root)?;

        // Collision check: local items shadow external items
        for item in &self_items {
            if target_state.items.contains_key(&item.dest_rel) {
                let existing = &target_state.items[&item.dest_rel];
                eprintln!(
                    "warning: local {} `{}` shadows dependency `{}` {} `{}`",
                    item.kind, item.name, existing.source_name, existing.id.kind, existing.id.name
                );
                // Remove external item from plan (it will be replaced by symlink)
                let dest_rel = item.dest_rel.clone();
                sync_plan
                    .actions
                    .retain(|a| !action_matches_dest(a, &dest_rel));
                target_state.items.shift_remove(&item.dest_rel);
            }
        }

        // Inject symlink actions for items that need updating
        for item in &self_items {
            let dest = managed_root.join(item.dest_rel.as_path());
            if !old_lock.items.contains_key(&item.dest_rel) && dest.symlink_metadata().is_ok() {
                eprintln!(
                    "warning: local {} `{}` collides with unmanaged path `{}` — leaving existing content untouched",
                    item.kind, item.name, item.dest_rel
                );
                skipped_self_dests.insert(item.dest_rel.clone());
                continue;
            }
            let needs_update = match dest.symlink_metadata() {
                Ok(meta) if meta.file_type().is_symlink() => {
                    let current_target = std::fs::read_link(&dest).ok();
                    let from_dir = dest.parent().unwrap();
                    let expected = pathdiff::diff_paths(&item.source_path, from_dir)
                        .unwrap_or_else(|| item.source_path.clone());
                    current_target.as_deref() != Some(expected.as_path())
                }
                Ok(_) => true,  // exists but not a symlink — replace
                Err(_) => true, // doesn't exist — create
            };
            if needs_update {
                sync_plan.actions.push(plan::PlannedAction::Symlink {
                    source_abs: item.source_path.clone(),
                    dest_rel: item.dest_rel.clone(),
                    kind: item.kind,
                    name: item.name.clone(),
                });
            }
        }

        // Prune old _self entries from lock that are no longer present
        let self_dest_set: std::collections::HashSet<_> =
            self_items.iter().map(|i| &i.dest_rel).collect();
        for (dest_path, locked_item) in &old_lock.items {
            if locked_item.source.as_ref() == "_self" && !self_dest_set.contains(dest_path) {
                sync_plan.actions.push(plan::PlannedAction::Remove {
                    locked: locked_item.clone(),
                });
            }
        }
    } else {
        // No [package] — prune any stale _self entries from lock
        for (_, locked_item) in &old_lock.items {
            if locked_item.source.as_ref() == "_self" {
                sync_plan.actions.push(plan::PlannedAction::Remove {
                    locked: locked_item.clone(),
                });
            }
        }
    }

    // Remove any orphan-removal actions targeting _self items.
    // The diff engine doesn't know about _self items, so it marks
    // old _self lock entries as orphans. We handle _self lifecycle explicitly
    // above (inject symlinks + explicit prune), so strip the diff engine's
    // Remove actions for _self items to prevent double-removal.
    sync_plan.actions.retain(|action| {
        if let plan::PlannedAction::Remove { locked } = action {
            locked.source.as_ref() != "_self"
        } else {
            true
        }
    });

    Ok(skipped_self_dests)
}

/// Check if a planned action targets a specific destination path.
fn action_matches_dest(action: &plan::PlannedAction, dest: &DestPath) -> bool {
    match action {
        plan::PlannedAction::Install { target } | plan::PlannedAction::Overwrite { target } => {
            &target.dest_path == dest
        }
        plan::PlannedAction::Skip { dest_path, .. }
        | plan::PlannedAction::KeepLocal { dest_path, .. } => dest_path == dest,
        plan::PlannedAction::Merge { target, .. } => &target.dest_path == dest,
        plan::PlannedAction::Remove { locked } => &locked.dest_path == dest,
        plan::PlannedAction::Symlink { dest_rel, .. } => dest_rel == dest,
    }
}
