use std::path::Path;

use crate::error::MarsError;
use crate::hash;
use crate::lock::{LockFile, LockedItem};
use crate::sync::target::{TargetItem, TargetState};
use crate::types::ContentHash;

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
        local_hash: ContentHash,
    },
    /// In lock but not in target → should be removed.
    Orphan { locked: LockedItem },
    /// Local modification, source unchanged → keep local.
    LocalModified {
        target: TargetItem,
        locked: LockedItem,
        local_hash: ContentHash,
    },
}

/// Compute the diff between current disk state + lock and target state.
///
/// Uses dual checksums from the lock file:
/// - `source_checksum`: what the source provided
/// - `installed_checksum`: what mars wrote to disk
///
/// Compares current disk hash against lock checksums to determine the diff entry variant.
pub fn compute(
    root: &Path,
    lock: &LockFile,
    target: &TargetState,
    force: bool,
) -> Result<SyncDiff, MarsError> {
    let mut items = Vec::new();

    // Process each target item
    for (_dest_key, target_item) in &target.items {
        if let Some(locked_item) = lock.items.get(&target_item.dest_path) {
            // Item exists in lock — compare checksums
            let source_changed = target_item.source_hash != locked_item.source_checksum;

            // Check disk hash against the expected baseline.
            // In --force mode, baseline is source_checksum so conflicted files
            // are treated as local modifications and get overwritten.
            let expected_disk_checksum = if force {
                &locked_item.source_checksum
            } else {
                &locked_item.installed_checksum
            };

            let disk_path = root.join(&target_item.dest_path);
            let local_changed = if disk_path.exists() {
                let disk_hash = hash::compute_hash(&disk_path, target_item.id.kind)?;
                let disk_hash = ContentHash::from(disk_hash);
                if disk_hash != *expected_disk_checksum {
                    Some(disk_hash)
                } else {
                    None
                }
            } else {
                // File was deleted locally — treat as if local changed to "nothing"
                // In this case, we should reinstall it
                None
            };

            match (source_changed, &local_changed) {
                (false, None) => {
                    // Neither changed → skip
                    if disk_path.exists() {
                        items.push(DiffEntry::Unchanged {
                            target: target_item.clone(),
                            locked: locked_item.clone(),
                        });
                    } else {
                        // File was deleted but hashes match lock — reinstall
                        items.push(DiffEntry::Add {
                            target: target_item.clone(),
                        });
                    }
                }
                (true, None) => {
                    // Source changed, local unchanged → clean update
                    items.push(DiffEntry::Update {
                        target: target_item.clone(),
                        locked: locked_item.clone(),
                    });
                }
                (false, Some(local_hash)) => {
                    // Local changed, source unchanged → keep local
                    items.push(DiffEntry::LocalModified {
                        target: target_item.clone(),
                        locked: locked_item.clone(),
                        local_hash: local_hash.clone(),
                    });
                }
                (true, Some(local_hash)) => {
                    // Both changed → conflict
                    items.push(DiffEntry::Conflict {
                        target: target_item.clone(),
                        locked: locked_item.clone(),
                        local_hash: local_hash.clone(),
                    });
                }
            }
        } else {
            // Not in lock → new item
            items.push(DiffEntry::Add {
                target: target_item.clone(),
            });
        }
    }

    // Find orphans: items in lock but not in target
    for (dest_path, locked_item) in &lock.items {
        if !target.items.contains_key(dest_path) {
            items.push(DiffEntry::Orphan {
                locked: locked_item.clone(),
            });
        }
    }

    Ok(SyncDiff { items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash;
    use crate::lock::{ItemId, ItemKind, LockedItem};
    use crate::types::{ItemName, SourceName};
    use indexmap::IndexMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create a minimal target item for testing.
    fn make_target_item(
        name: &str,
        kind: ItemKind,
        source_hash: &str,
        source_path: PathBuf,
    ) -> TargetItem {
        let dest_path = match kind {
            ItemKind::Agent => PathBuf::from("agents").join(format!("{name}.md")),
            ItemKind::Skill => PathBuf::from("skills").join(name),
        };
        TargetItem {
            id: ItemId {
                kind,
                name: ItemName::from(name),
            },
            source_name: SourceName::from("test-source"),
            origin: crate::types::SourceOrigin::Dependency(SourceName::from("test-source")),
            source_id: crate::types::SourceId::Path {
                canonical: source_path.clone(),
            },
            source_path,
            dest_path: dest_path.into(),
            source_hash: ContentHash::from(source_hash),
            is_flat_skill: false,
            rewritten_content: None,
        }
    }

    fn make_locked_item(
        name: &str,
        kind: ItemKind,
        source_checksum: &str,
        installed_checksum: &str,
    ) -> LockedItem {
        let dest_path = match kind {
            ItemKind::Agent => format!("agents/{name}.md"),
            ItemKind::Skill => format!("skills/{name}"),
        };
        LockedItem {
            source: SourceName::from("test-source"),
            kind,
            version: None,
            source_checksum: ContentHash::from(source_checksum),
            installed_checksum: ContentHash::from(installed_checksum),
            dest_path: dest_path.into(),
        }
    }

    #[test]
    fn new_item_produces_add() {
        let root = TempDir::new().unwrap();
        let source_dir = TempDir::new().unwrap();
        let source_path = source_dir.path().join("agents/coder.md");
        fs::create_dir_all(source_dir.path().join("agents")).unwrap();
        fs::write(&source_path, "# new agent").unwrap();

        let hash = hash::hash_bytes(b"# new agent");

        let target_item = make_target_item("coder", ItemKind::Agent, &hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        let lock = LockFile::empty();
        let diff = compute(root.path(), &lock, &target, false).unwrap();

        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::Add { .. }));
    }

    #[test]
    fn unchanged_item_produces_unchanged() {
        let root = TempDir::new().unwrap();
        let content = b"# existing agent";
        let hash = hash::hash_bytes(content);

        // Write file to disk
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), content).unwrap();

        let source_path = PathBuf::from("/tmp/source/agents/coder.md");

        let target_item = make_target_item("coder", ItemKind::Agent, &hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        let locked_item = make_locked_item("coder", ItemKind::Agent, &hash, &hash);
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/coder.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::Unchanged { .. }));
    }

    #[test]
    fn source_changed_local_unchanged_produces_update() {
        let root = TempDir::new().unwrap();
        let old_content = b"# old version";
        let old_hash = hash::hash_bytes(old_content);
        let new_hash = hash::hash_bytes(b"# new version");

        // Write old content to disk (matching lock's installed_checksum)
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), old_content).unwrap();

        let source_path = PathBuf::from("/tmp/source/agents/coder.md");

        // Target has new hash
        let target_item = make_target_item("coder", ItemKind::Agent, &new_hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        // Lock has old hash
        let locked_item = make_locked_item("coder", ItemKind::Agent, &old_hash, &old_hash);
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/coder.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::Update { .. }));
    }

    #[test]
    fn local_changed_source_unchanged_produces_local_modified() {
        let root = TempDir::new().unwrap();
        let original_content = b"# original";
        let original_hash = hash::hash_bytes(original_content);
        let local_content = b"# locally modified";

        // Write locally modified content to disk
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), local_content).unwrap();

        let source_path = PathBuf::from("/tmp/source/agents/coder.md");

        // Target has same source hash as lock (no upstream change)
        let target_item = make_target_item("coder", ItemKind::Agent, &original_hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        // Lock also has original hash
        let locked_item =
            make_locked_item("coder", ItemKind::Agent, &original_hash, &original_hash);
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/coder.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::LocalModified { .. }));
    }

    #[test]
    fn both_changed_produces_conflict() {
        let root = TempDir::new().unwrap();
        let original_hash = hash::hash_bytes(b"# original");
        let new_source_hash = hash::hash_bytes(b"# new upstream");
        let local_content = b"# locally modified";

        // Write locally modified content
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), local_content).unwrap();

        let source_path = PathBuf::from("/tmp/source/agents/coder.md");

        // Target has new source hash (upstream changed)
        let target_item = make_target_item("coder", ItemKind::Agent, &new_source_hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        // Lock has original hash
        let locked_item =
            make_locked_item("coder", ItemKind::Agent, &original_hash, &original_hash);
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/coder.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::Conflict { .. }));
    }

    #[test]
    fn orphan_detected() {
        let root = TempDir::new().unwrap();

        // Empty target — no items wanted
        let target = TargetState {
            items: IndexMap::new(),
        };

        // Lock has an item
        let locked_item =
            make_locked_item("old-agent", ItemKind::Agent, "sha256:aaa", "sha256:aaa");
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/old-agent.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        assert!(matches!(&diff.items[0], DiffEntry::Orphan { .. }));
    }

    #[test]
    fn dual_checksum_prevents_false_conflict() {
        // When mars rewrites frontmatter, source_checksum != installed_checksum.
        // The disk should match installed_checksum (what mars wrote).
        // This should NOT be detected as a local modification.
        let root = TempDir::new().unwrap();

        let source_hash = hash::hash_bytes(b"# original source");
        let installed_content = b"# rewritten by mars";
        let installed_hash = hash::hash_bytes(installed_content);

        // Disk has the mars-rewritten content
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), installed_content).unwrap();

        let source_path = PathBuf::from("/tmp/source/agents/coder.md");

        // Target has same source hash as before (no upstream change)
        let target_item = make_target_item("coder", ItemKind::Agent, &source_hash, source_path);
        let mut target_items = IndexMap::new();
        target_items.insert("agents/coder.md".into(), target_item);
        let target = TargetState {
            items: target_items,
        };

        // Lock has different source_checksum and installed_checksum
        let locked_item = make_locked_item("coder", ItemKind::Agent, &source_hash, &installed_hash);
        let mut lock_items = IndexMap::new();
        lock_items.insert("agents/coder.md".into(), locked_item);
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 1);
        // Should be Unchanged because disk matches installed_checksum
        // and source_hash matches source_checksum
        assert!(
            matches!(&diff.items[0], DiffEntry::Unchanged { .. }),
            "expected Unchanged, got {:?}",
            diff.items[0]
        );
    }

    #[test]
    fn mixed_diff_entries() {
        let root = TempDir::new().unwrap();
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        let hash_a = hash::hash_bytes(b"# unchanged");
        let hash_b_old = hash::hash_bytes(b"# old version");
        let hash_b_new = hash::hash_bytes(b"# new version");

        // Write unchanged file
        fs::write(agents_dir.join("stable.md"), b"# unchanged").unwrap();

        // Write file with old content (will be updated)
        fs::write(agents_dir.join("updating.md"), b"# old version").unwrap();

        let source_path_a = PathBuf::from("/tmp/source/agents/stable.md");
        let source_path_b = PathBuf::from("/tmp/source/agents/updating.md");
        let source_path_c = PathBuf::from("/tmp/source/agents/new.md");

        let mut target_items = IndexMap::new();
        target_items.insert(
            "agents/stable.md".into(),
            make_target_item("stable", ItemKind::Agent, &hash_a, source_path_a),
        );
        target_items.insert(
            "agents/updating.md".into(),
            make_target_item("updating", ItemKind::Agent, &hash_b_new, source_path_b),
        );
        target_items.insert(
            "agents/new.md".into(),
            make_target_item(
                "new",
                ItemKind::Agent,
                &hash::hash_bytes(b"# brand new"),
                source_path_c,
            ),
        );
        let target = TargetState {
            items: target_items,
        };

        let mut lock_items = IndexMap::new();
        lock_items.insert(
            "agents/stable.md".into(),
            make_locked_item("stable", ItemKind::Agent, &hash_a, &hash_a),
        );
        lock_items.insert(
            "agents/updating.md".into(),
            make_locked_item("updating", ItemKind::Agent, &hash_b_old, &hash_b_old),
        );
        lock_items.insert(
            "agents/orphan.md".into(),
            make_locked_item("orphan", ItemKind::Agent, "sha256:xxx", "sha256:xxx"),
        );
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let diff = compute(root.path(), &lock, &target, false).unwrap();
        assert_eq!(diff.items.len(), 4); // Unchanged + Update + Add + Orphan

        let unchanged_count = diff
            .items
            .iter()
            .filter(|d| matches!(d, DiffEntry::Unchanged { .. }))
            .count();
        let update_count = diff
            .items
            .iter()
            .filter(|d| matches!(d, DiffEntry::Update { .. }))
            .count();
        let add_count = diff
            .items
            .iter()
            .filter(|d| matches!(d, DiffEntry::Add { .. }))
            .count();
        let orphan_count = diff
            .items
            .iter()
            .filter(|d| matches!(d, DiffEntry::Orphan { .. }))
            .count();

        assert_eq!(unchanged_count, 1);
        assert_eq!(update_count, 1);
        assert_eq!(add_count, 1);
        assert_eq!(orphan_count, 1);
    }

    #[test]
    fn force_uses_source_checksum_for_local_change_detection() {
        let root = TempDir::new().unwrap();
        let upstream_content = b"# upstream";
        let conflicted_content = b"<<<<<<< local\n# local\n=======\n# upstream\n>>>>>>> upstream\n";

        let source_hash = hash::hash_bytes(upstream_content);
        let installed_hash = hash::hash_bytes(conflicted_content);

        // Disk matches prior conflicted content from last sync.
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), conflicted_content).unwrap();

        let mut target_items = IndexMap::new();
        target_items.insert(
            "agents/coder.md".into(),
            make_target_item(
                "coder",
                ItemKind::Agent,
                &source_hash,
                PathBuf::from("/tmp/source/agents/coder.md"),
            ),
        );
        let target = TargetState {
            items: target_items,
        };

        let mut lock_items = IndexMap::new();
        lock_items.insert(
            "agents/coder.md".into(),
            LockedItem {
                source: "test-source".into(),
                kind: ItemKind::Agent,
                version: None,
                source_checksum: source_hash.clone().into(),
                installed_checksum: installed_hash.into(),
                dest_path: "agents/coder.md".into(),
            },
        );
        let lock = LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: lock_items,
        };

        let normal = compute(root.path(), &lock, &target, false).unwrap();
        assert!(matches!(&normal.items[0], DiffEntry::Unchanged { .. }));

        let forced = compute(root.path(), &lock, &target, true).unwrap();
        assert!(matches!(&forced.items[0], DiffEntry::LocalModified { .. }));
    }
}
