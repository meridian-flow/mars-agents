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

use std::path::Path;

use crate::error::MarsError;
use crate::link::{self, ConflictInfo, ScanResult};

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

/// Run `mars link`.
pub fn run(args: &LinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    if args.unlink {
        let target_name = link::normalize_link_target(&args.target)?;
        let target_dir = ctx.project_root.join(&target_name);
        return unlink(ctx, &target_name, &target_dir, json);
    }

    let target_name = link::normalize_link_target(&args.target)?;
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
    let mars_dir = ctx.project_root.join(".mars");
    std::fs::create_dir_all(&mars_dir)?;
    let lock_path = mars_dir.join("sync.lock");
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
    let mut foreign_details: Vec<(&str, std::path::PathBuf)> = Vec::new();

    for subdir in ["agents", "skills"] {
        let link_path = target_dir.join(subdir);
        let link_target = rel_root.join(subdir);
        let managed_subdir = ctx.managed_root.join(subdir);

        let result = link::scan_link_target(&link_path, &managed_subdir);
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
        print_conflicts(ctx, &target_name, &all_conflicts, &foreign_details, json);
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
                link::create_symlink(&link_path, &link_target)?;
                linked += 1;
            }
            ScanResult::AlreadyLinked => {
                if !json {
                    output::print_info(&format!("{target_name}/{subdir} already linked"));
                }
            }
            ScanResult::MergeableDir { files_to_move } => {
                let managed_subdir = ctx.managed_root.join(subdir);
                link::merge_and_link(&link_path, &link_target, &managed_subdir, &files_to_move)?;
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
                link::create_symlink(&link_path, &link_target)?;
                linked += 1;
            }
        }
    }

    // Persist link in config (already under sync lock from above).
    let mut config = crate::config::load(&ctx.project_root)?;
    let mut changed = false;

    // Add to targets if targets is explicitly set
    if let Some(ref mut targets) = config.settings.targets
        && !targets.contains(&target_name)
    {
        targets.push(target_name.clone());
        changed = true;
    }

    // Also maintain legacy links for backward compat
    if !config.settings.links.contains(&target_name) {
        config.settings.links.push(target_name.clone());
        changed = true;
    }

    if changed {
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

/// Format and print conflict details.
fn print_conflicts(
    ctx: &super::MarsContext,
    target_name: &str,
    all_conflicts: &[(&str, ConflictInfo)],
    foreign_details: &[(&str, std::path::PathBuf)],
    json: bool,
) {
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
        for (subdir, info) in all_conflicts {
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
        for (subdir, foreign_target) in foreign_details {
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
}

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
        ctx,
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
