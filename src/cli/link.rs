//! `mars link <dir>` — symlink agents/ and skills/ into another directory.
//!
//! Creates `<dir>/agents -> <mars-root>/agents` and `<dir>/skills -> <mars-root>/skills`.
//! Useful for tools that look in `.claude/`, `.cursor/`, etc. instead of `.agents/`.
//!
//! Uses a conflict-aware scan-then-act algorithm:
//! - Phase 1 (scan): read-only analysis of the target directory
//! - Phase 2 (act): filesystem mutations only if scan found no conflicts
//!
//! If any conflict exists, zero mutations occur. The user sees all problems at once.
//!
//! Persists the link in `mars.toml [settings] links` so `mars doctor` can verify it.

use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::hash;

use super::output;

/// Arguments for `mars link`.
#[derive(Debug, clap::Args)]
pub struct LinkArgs {
    /// Target directory to create symlinks in (e.g. `.claude`).
    pub target: String,

    /// Remove symlinks instead of creating them.
    #[arg(long)]
    pub unlink: bool,

    /// Replace whatever exists with symlinks. Data may be lost.
    #[arg(long)]
    pub force: bool,
}

/// Result of scanning a single subdir (agents/ or skills/) in the target.
enum ScanResult {
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
struct ConflictInfo {
    /// Relative path within the subdir (e.g. "reviewer.md").
    relative_path: PathBuf,
    /// Description of what exists in the target dir.
    target_desc: String,
    /// Description of what exists in the managed root.
    managed_desc: String,
}

/// Run `mars link`.
pub fn run(args: &LinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    if args.unlink {
        let target_name = normalize_link_target(&args.target)?;
        let target_dir = ctx.project_root.join(&target_name);
        return unlink(ctx, &target_name, &target_dir, json);
    }

    let target_name = normalize_link_target(&args.target)?;
    let target_dir = ctx.project_root.join(&target_name);

    // Reject self-link — linking the managed root to itself creates circular symlinks
    if let (Ok(target_canon), Ok(root_canon)) = (
        target_dir
            .canonicalize()
            .or_else(|_| Ok::<_, std::io::Error>(target_dir.clone())),
        ctx.managed_root.canonicalize(),
    ) && target_canon == root_canon
    {
        return Err(MarsError::Link {
            target: target_name,
            message: "cannot link the managed root to itself".to_string(),
        });
    }

    // Verify config exists before any mutations (resolve-first principle)
    let config_path = ctx.project_root.join("mars.toml");
    if !config_path.exists() {
        return Err(MarsError::Link {
            target: target_name,
            message: format!(
                "mars.toml not found at {} — run `mars init` first",
                ctx.project_root.display()
            ),
        });
    }

    // Warn if target isn't a well-known tool dir
    if !json
        && !super::WELL_KNOWN.contains(&target_name.as_str())
        && !super::TOOL_DIRS.contains(&target_name.as_str())
    {
        output::print_warn(&format!(
            "`{target_name}` is not a recognized tool directory — linking anyway"
        ));
    }

    // Acquire sync lock for the entire operation (scan + act + persist).
    // Prevents races with concurrent mars sync or mars link.
    let lock_path = ctx.managed_root.join(".mars").join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    // Create target directory if needed
    std::fs::create_dir_all(&target_dir)?;

    // Ensure managed subdirs exist
    for subdir in ["agents", "skills"] {
        let source = ctx.managed_root.join(subdir);
        if !source.exists() {
            std::fs::create_dir_all(&source)?;
        }
    }

    // Compute relative path from target dir back to mars root
    let rel_root = pathdiff::diff_paths(&ctx.managed_root, &target_dir)
        .unwrap_or_else(|| ctx.managed_root.clone());

    // ── Phase 1: Scan all subdirs ──────────────────────────────────────────
    let mut scan_results = Vec::new();
    let mut all_conflicts: Vec<(&str, ConflictInfo)> = Vec::new();
    let mut has_foreign = false;
    let mut foreign_details: Vec<(&str, PathBuf)> = Vec::new();

    for subdir in ["agents", "skills"] {
        let link_path = target_dir.join(subdir);
        let link_target = rel_root.join(subdir);
        let managed_subdir = ctx.managed_root.join(subdir);

        let result = scan_link_target(&link_path, &managed_subdir);
        match &result {
            ScanResult::ConflictedDir { conflicts } => {
                for c in conflicts {
                    all_conflicts.push((subdir, c.clone()));
                }
            }
            ScanResult::ForeignSymlink { target } => {
                has_foreign = true;
                foreign_details.push((subdir, target.clone()));
            }
            _ => {}
        }
        scan_results.push((subdir, link_path, link_target, result));
    }

    // Check: any conflicts or foreign symlinks? (unless --force)
    if !args.force && (!all_conflicts.is_empty() || has_foreign) {
        if json {
            let conflict_json: Vec<_> = all_conflicts
                .iter()
                .map(|(subdir, c)| {
                    serde_json::json!({
                        "path": format!("{}/{}", subdir, c.relative_path.display()),
                        "target_desc": c.target_desc,
                        "managed_desc": c.managed_desc,
                    })
                })
                .collect();
            output::print_json(&serde_json::json!({
                "ok": false,
                "error": "conflicts found",
                "conflicts": conflict_json,
            }));
        } else {
            let total = all_conflicts.len() + foreign_details.len();
            eprintln!("error: cannot link {target_name} — {total} conflict(s) found:\n");
            for (subdir, info) in &all_conflicts {
                eprintln!("  {subdir}/{}", info.relative_path.display());
                eprintln!(
                    "    {target_name}/{subdir}/{} ({})",
                    info.relative_path.display(),
                    info.target_desc
                );
                eprintln!(
                    "    {}/{subdir}/{} ({})\n",
                    ctx.managed_root
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    info.relative_path.display(),
                    info.managed_desc
                );
            }
            for (subdir, foreign_target) in &foreign_details {
                eprintln!(
                    "  {target_name}/{subdir} is a symlink to {} (not this mars root)\n",
                    foreign_target.display()
                );
            }
            eprintln!("hint: resolve conflicts manually, then retry `mars link {target_name}`");
            eprintln!(
                "hint: or use `mars link {target_name} --force` to replace with symlinks (data loss)"
            );
        }
        return Err(MarsError::Link {
            target: target_name,
            message: "conflicts found — resolve manually or use --force".to_string(),
        });
    }

    // ── Phase 2: Act ───────────────────────────────────────────────────────
    let mut linked = 0;
    for (subdir, link_path, link_target, result) in scan_results {
        match result {
            ScanResult::Empty => {
                create_symlink(&link_path, &link_target)?;
                linked += 1;
            }
            ScanResult::AlreadyLinked => {
                if !json {
                    output::print_info(&format!("{target_name}/{subdir} already linked"));
                }
            }
            ScanResult::MergeableDir { files_to_move } => {
                let managed_subdir = ctx.managed_root.join(subdir);
                merge_and_link(&link_path, &link_target, &managed_subdir, &files_to_move)?;
                linked += 1;
                if !json && !files_to_move.is_empty() {
                    output::print_info(&format!(
                        "merged {} file(s) from {target_name}/{subdir} into managed root",
                        files_to_move.len()
                    ));
                }
            }
            ScanResult::ForeignSymlink { .. } | ScanResult::ConflictedDir { .. } => {
                // Only reachable with --force
                if link_path.symlink_metadata().is_ok() {
                    if link_path.read_link().is_ok() {
                        std::fs::remove_file(&link_path)?;
                    } else {
                        std::fs::remove_dir_all(&link_path)?;
                    }
                }
                create_symlink(&link_path, &link_target)?;
                linked += 1;
            }
        }
    }

    // Persist link in config (already under sync lock from above).
    let mut config = crate::config::load(&ctx.project_root)?;
    if !config.settings.links.contains(&target_name) {
        config.settings.links.push(target_name.clone());
        crate::config::save(&ctx.project_root, &config)?;
    }

    // Output
    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "target": target_dir.to_string_lossy(),
            "linked": linked,
        }));
    } else if linked > 0 {
        output::print_success(&format!("linked agents/ and skills/ into {target_name}"));
    } else {
        output::print_info(&format!("{target_name} already fully linked"));
    }

    Ok(0)
}

// ── Scan ────────────────────────────────────────────────────────────────────

/// Scan a single link target (e.g. `.claude/agents/`) to determine its state.
fn scan_link_target(link_path: &Path, managed_subdir: &Path) -> ScanResult {
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
fn scan_dir_recursive(target_subdir: &Path, managed_subdir: &Path) -> ScanResult {
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

// ── Act ─────────────────────────────────────────────────────────────────────

/// Move unique files into managed root, remove the target dir, create symlink.
fn merge_and_link(
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
fn remove_dir_contents_and_tree(dir: &Path) -> Result<(), MarsError> {
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
fn create_symlink(link_path: &Path, link_target: &Path) -> Result<(), MarsError> {
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

// ── Unlink ──────────────────────────────────────────────────────────────────

/// Remove symlinks created by `mars link`.
/// Only removes symlinks that point to THIS mars root.
fn unlink(
    ctx: &super::MarsContext,
    target_name: &str,
    target_dir: &Path,
    json: bool,
) -> Result<i32, MarsError> {
    let mut removed = 0;

    for subdir in ["agents", "skills"] {
        let link_path = target_dir.join(subdir);

        if let Ok(link_target) = link_path.read_link() {
            // Resolve the symlink target to absolute and compare
            let resolved = target_dir.join(&link_target);
            let expected = ctx.managed_root.join(subdir);

            // Both must canonicalize successfully AND match.
            let matches = match (resolved.canonicalize(), expected.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };

            if matches {
                std::fs::remove_file(&link_path)?;
                removed += 1;
            } else if !json {
                output::print_warn(&format!(
                    "{target_name}/{subdir} is a symlink to {} (not this mars root) — skipping",
                    link_target.display()
                ));
            }
        }
    }

    // Remove from settings (under sync lock)
    crate::sync::mutate_link_config(
        &ctx.project_root,
        &ctx.managed_root,
        &crate::sync::LinkMutation::Clear {
            target: target_name.to_string(),
        },
    )?;

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "removed": removed,
        }));
    } else if removed > 0 {
        output::print_success(&format!("removed {removed} symlink(s) from {target_name}"));
    } else {
        output::print_info("no symlinks to remove");
    }

    Ok(0)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Normalize and validate a link target name.
fn normalize_link_target(target: &str) -> Result<String, MarsError> {
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
