//! Link operations — scan/act logic for symlinking agents/ and skills/.
//!
//! This module contains the filesystem operations for `mars link`:
//! - Scanning target directories for conflicts
//! - Merging non-conflicting files into the managed root
//! - Creating/removing symlinks
//!
//! The CLI layer (`cli::link`) handles argument parsing and output formatting.

use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::hash;

/// Result of scanning a single subdir (agents/ or skills/) in the target.
pub(crate) enum ScanResult {
    /// Nothing at the link path — create symlink.
    Empty,
    /// Already a symlink pointing to our managed root.
    AlreadyLinked,
    /// Symlink pointing somewhere else.
    ForeignSymlink { target: PathBuf },
    /// Real directory with no conflicts against managed root.
    MergeableDir { files_to_move: Vec<PathBuf> },
    /// Real directory with conflicts (same filename, different content).
    ConflictedDir { conflicts: Vec<ConflictInfo> },
}

/// Details about a single file conflict between target and managed root.
#[derive(Clone)]
pub(crate) struct ConflictInfo {
    /// Relative path within the subdir (e.g. "reviewer.md").
    pub relative_path: PathBuf,
    /// Description of what exists in the target dir.
    pub target_desc: String,
    /// Description of what exists in the managed root.
    pub managed_desc: String,
}

/// Scan a single link target (e.g. `.claude/agents/`) to determine its state.
pub(crate) fn scan_link_target(link_path: &Path, managed_subdir: &Path) -> ScanResult {
    // Check if anything exists at link_path
    if link_path.symlink_metadata().is_err() {
        return ScanResult::Empty;
    }

    // Check if it's a symlink
    if let Ok(actual_target) = link_path.read_link() {
        // Use canonicalize for comparison — textually different but semantically
        // identical paths should match.
        let actual_resolved = link_path
            .parent()
            .map(|p| p.join(&actual_target))
            .and_then(|p| p.canonicalize().ok());
        let expected_resolved = managed_subdir.canonicalize().ok();

        match (actual_resolved, expected_resolved) {
            (Some(a), Some(b)) if a == b => return ScanResult::AlreadyLinked,
            _ => {
                return ScanResult::ForeignSymlink {
                    target: actual_target,
                };
            }
        }
    }

    // It's a real directory — scan recursively
    scan_dir_recursive(link_path, managed_subdir)
}

/// Recursively scan a target directory against the managed root.
pub(crate) fn scan_dir_recursive(target_subdir: &Path, managed_subdir: &Path) -> ScanResult {
    let mut files_to_move = Vec::new();
    let mut conflicts = Vec::new();

    // Walk the target directory recursively, without following symlinks
    for entry in walkdir::WalkDir::new(target_subdir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let ft = entry.file_type();
        if ft.is_dir() {
            continue; // Directories are structural, handled during cleanup
        }
        if ft.is_symlink() {
            // Skip symlinks — don't follow, don't treat as conflicts.
            // They survive the merge-and-link process since we only
            // remove regular files.
            continue;
        }

        let relative = match entry.path().strip_prefix(target_subdir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };

        // Non-regular files (fifos, sockets) → treat as conflict
        if !ft.is_file() {
            conflicts.push(ConflictInfo {
                relative_path: relative,
                target_desc: format!("<non-regular: {:?}>", ft),
                managed_desc: String::new(),
            });
            continue;
        }

        let managed_path = managed_subdir.join(&relative);

        if !managed_path.exists() {
            // Unique file — can be moved
            files_to_move.push(relative);
        } else if managed_path.is_file() {
            // Both exist as files — compare content
            let target_hash = hash_file(entry.path());
            let managed_hash = hash_file(&managed_path);
            match (target_hash, managed_hash) {
                (Some(th), Some(mh)) if th == mh => {
                    // Identical — skip
                }
                (Some(th), Some(mh)) => {
                    conflicts.push(ConflictInfo {
                        relative_path: relative,
                        target_desc: th,
                        managed_desc: mh,
                    });
                }
                (th, mh) => {
                    // Can't read one or both files — treat as conflict
                    conflicts.push(ConflictInfo {
                        relative_path: relative,
                        target_desc: th.unwrap_or_else(|| "unreadable".to_string()),
                        managed_desc: mh.unwrap_or_else(|| "unreadable".to_string()),
                    });
                }
            }
        } else {
            // Type mismatch (file in target, dir in managed or vice versa)
            conflicts.push(ConflictInfo {
                relative_path: relative,
                target_desc: "file".to_string(),
                managed_desc: "directory".to_string(),
            });
        }
    }

    if !conflicts.is_empty() {
        ScanResult::ConflictedDir { conflicts }
    } else {
        ScanResult::MergeableDir { files_to_move }
    }
}

/// Compute SHA-256 of a single file for comparison.
/// Returns None if the file can't be read (permission denied, etc).
fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| hash::hash_bytes(&bytes))
}

/// Move unique files into managed root, remove the target dir, create symlink.
pub(crate) fn merge_and_link(
    link_path: &Path,
    link_target: &Path,
    managed_subdir: &Path,
    files_to_move: &[PathBuf],
) -> Result<(), MarsError> {
    // Move unique files into managed root (copy+delete for cross-fs safety)
    for relative in files_to_move {
        let src = link_path.join(relative);
        let dst = managed_subdir.join(relative);

        // Create parent dirs in managed root if needed
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::copy(&src, &dst).map_err(|e| MarsError::Link {
            target: link_path.display().to_string(),
            message: format!("failed to copy {}: {e}", relative.display()),
        })?;
        std::fs::remove_file(&src)?;
    }

    // Remove remaining files (identical copies we skipped during scan)
    // and clean up directory tree bottom-up
    remove_dir_contents_and_tree(link_path)?;

    // Create symlink
    create_symlink(link_path, link_target)
}

/// Remove all remaining files in a directory, then remove empty dirs bottom-up.
/// Uses remove_dir (not remove_dir_all) so non-empty dirs fail safely.
pub(crate) fn remove_dir_contents_and_tree(dir: &Path) -> Result<(), MarsError> {
    // First, remove all regular files
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        std::fs::remove_file(entry.path())?;
    }

    // Then, remove empty directories bottom-up (deepest first)
    let mut dirs: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    dirs.sort_by(|a, b| b.cmp(a)); // Reverse order = deepest first

    for d in dirs {
        // remove_dir fails if non-empty — that's the safety net
        let _ = std::fs::remove_dir(&d);
    }

    Ok(())
}

/// Create a symlink. Unix-only.
pub(crate) fn create_symlink(link_path: &Path, link_target: &Path) -> Result<(), MarsError> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(link_target, link_path).map_err(|e| MarsError::Link {
            target: link_path.display().to_string(),
            message: format!(
                "failed to create symlink {} -> {}: {e}",
                link_path.display(),
                link_target.display()
            ),
        })?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = (link_path, link_target);
        Err(MarsError::Link {
            target: String::new(),
            message: "symlinks are only supported on Unix".to_string(),
        })
    }
}

/// Normalize and validate a link target name.
pub(crate) fn normalize_link_target(target: &str) -> Result<String, MarsError> {
    let normalized = target.trim_end_matches('/').trim_end_matches('\\');
    if normalized.contains('/') || normalized.contains('\\') {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "link target must be a directory name, not a path".to_string(),
        });
    }
    if normalized.is_empty() || normalized == "." || normalized == ".." {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "invalid link target name".to_string(),
        });
    }
    Ok(normalized.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize_link_target(".claude/").unwrap(), ".claude");
    }

    #[test]
    fn normalize_rejects_path() {
        assert!(normalize_link_target("foo/bar").is_err());
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_link_target("").is_err());
    }

    #[test]
    fn normalize_rejects_dots() {
        assert!(normalize_link_target(".").is_err());
        assert!(normalize_link_target("..").is_err());
    }

    #[test]
    fn scan_empty_returns_empty() {
        let dir = TempDir::new().unwrap();
        let link_path = dir.path().join("agents");
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        // link_path doesn't exist
        let result = scan_link_target(&link_path, &managed);
        assert!(matches!(result, ScanResult::Empty));
    }

    #[test]
    fn scan_symlink_to_correct_target_returns_already_linked() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();

        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let link_path = target_dir.join("agents");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&managed, &link_path).unwrap();

        let result = scan_link_target(&link_path, &managed);
        assert!(matches!(result, ScanResult::AlreadyLinked));
    }

    #[test]
    fn scan_symlink_to_wrong_target_returns_foreign() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();

        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();

        let target_dir = dir.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let link_path = target_dir.join("agents");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, &link_path).unwrap();

        let result = scan_link_target(&link_path, &managed);
        assert!(matches!(result, ScanResult::ForeignSymlink { .. }));
    }

    #[test]
    fn scan_dir_with_unique_files_returns_mergeable() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();

        let target_sub = dir.path().join("target_sub");
        std::fs::create_dir_all(&target_sub).unwrap();
        std::fs::write(target_sub.join("unique.md"), "unique content").unwrap();

        let result = scan_dir_recursive(&target_sub, &managed);
        match result {
            ScanResult::MergeableDir { files_to_move } => {
                assert_eq!(files_to_move.len(), 1);
                assert_eq!(files_to_move[0], PathBuf::from("unique.md"));
            }
            _ => panic!("expected MergeableDir"),
        }
    }

    #[test]
    fn scan_dir_with_identical_files_returns_mergeable_empty() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("same.md"), "content").unwrap();

        let target_sub = dir.path().join("target_sub");
        std::fs::create_dir_all(&target_sub).unwrap();
        std::fs::write(target_sub.join("same.md"), "content").unwrap();

        let result = scan_dir_recursive(&target_sub, &managed);
        match result {
            ScanResult::MergeableDir { files_to_move } => {
                assert!(files_to_move.is_empty());
            }
            _ => panic!("expected MergeableDir with empty files_to_move"),
        }
    }

    #[test]
    fn scan_dir_with_conflicting_files_returns_conflicted() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("conflict.md"), "managed version").unwrap();

        let target_sub = dir.path().join("target_sub");
        std::fs::create_dir_all(&target_sub).unwrap();
        std::fs::write(target_sub.join("conflict.md"), "target version").unwrap();

        let result = scan_dir_recursive(&target_sub, &managed);
        match result {
            ScanResult::ConflictedDir { conflicts } => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].relative_path, PathBuf::from("conflict.md"));
            }
            _ => panic!("expected ConflictedDir"),
        }
    }

    #[test]
    fn scan_dir_recursive_handles_nested() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(managed.join("sub")).unwrap();
        std::fs::write(managed.join("sub").join("existing.md"), "managed").unwrap();

        let target_sub = dir.path().join("target_sub");
        std::fs::create_dir_all(target_sub.join("sub")).unwrap();
        std::fs::write(target_sub.join("sub").join("existing.md"), "different").unwrap();
        std::fs::write(target_sub.join("sub").join("unique.md"), "unique").unwrap();

        let result = scan_dir_recursive(&target_sub, &managed);
        match result {
            ScanResult::ConflictedDir { conflicts } => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].relative_path, PathBuf::from("sub/existing.md"));
            }
            _ => panic!("expected ConflictedDir"),
        }
    }

    #[test]
    fn merge_and_link_moves_files_and_creates_symlink() {
        let dir = TempDir::new().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();

        let target_dir = dir.path().join("target");
        let target_sub = target_dir.join("agents");
        std::fs::create_dir_all(&target_sub).unwrap();
        std::fs::write(target_sub.join("unique.md"), "content").unwrap();

        let link_target = PathBuf::from("../managed");
        let files = vec![PathBuf::from("unique.md")];

        merge_and_link(&target_sub, &link_target, &managed, &files).unwrap();

        // File should be in managed root
        assert!(managed.join("unique.md").exists());
        // target_sub should be a symlink now
        assert!(
            target_sub
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn scan_dir_recursive_skips_symlinks() {
        let dir = TempDir::new().unwrap();
        let target_sub = dir.path().join("target").join("agents");
        let managed = dir.path().join("managed").join("agents");
        std::fs::create_dir_all(&target_sub).unwrap();
        std::fs::create_dir_all(&managed).unwrap();

        // Regular file — not a conflict (unique to target)
        std::fs::write(target_sub.join("real.md"), "real agent").unwrap();

        // Symlink in target dir — should be skipped, not treated as conflict
        std::os::unix::fs::symlink(target_sub.join("real.md"), target_sub.join("linked.md"))
            .unwrap();

        let result = scan_dir_recursive(&target_sub, &managed);
        match result {
            ScanResult::MergeableDir { files_to_move } => {
                // Only the real file should be listed for moving
                assert_eq!(files_to_move.len(), 1);
                assert_eq!(files_to_move[0], PathBuf::from("real.md"));
            }
            _ => panic!(
                "expected MergeableDir, got {:?}",
                std::mem::discriminant(&result)
            ),
        }
    }

    #[test]
    fn remove_dir_contents_and_tree_cleans_up() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        std::fs::write(target.join("a.md"), "a").unwrap();
        std::fs::write(target.join("sub").join("b.md"), "b").unwrap();

        remove_dir_contents_and_tree(&target).unwrap();

        assert!(!target.exists());
    }
}
