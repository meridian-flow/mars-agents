//! `mars resolve` — mark conflicts as resolved after user fixes them.

use std::path::PathBuf;

use crate::error::MarsError;
use crate::hash;
use crate::types::{ContentHash, DestPath};

use super::output;

/// Arguments for `mars resolve`.
#[derive(Debug, clap::Args)]
pub struct ResolveArgs {
    /// Specific file to resolve (default: all conflicted).
    pub file: Option<PathBuf>,
}

/// Run `mars resolve`.
pub fn run(args: &ResolveArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars_dir = ctx.project_root.join(".mars");
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut lock = crate::lock::load(&ctx.project_root)?;
    let mut resolved_files = Vec::new();
    let mut still_conflicted = Vec::new();

    let items_to_check: Vec<DestPath> = if let Some(file) = &args.file {
        // Check specific file
        let rel_input = if file.is_absolute() {
            file.strip_prefix(&mars_dir).map_or_else(
                |_| file.to_string_lossy().to_string(),
                |r| r.to_string_lossy().to_string(),
            )
        } else {
            file.to_string_lossy().to_string()
        };
        let rel = DestPath::new(&rel_input).map_err(|e| MarsError::Source {
            source_name: "resolve".to_string(),
            message: format!("invalid managed item path `{}`: {e}", file.display()),
        })?;
        if lock.items.contains_key(&rel) {
            vec![rel]
        } else {
            return Err(MarsError::Source {
                source_name: "resolve".to_string(),
                message: format!("{} is not a managed item", file.display()),
            });
        }
    } else {
        // Check all items
        lock.items.keys().cloned().collect()
    };

    for dest_path_str in &items_to_check {
        let disk_path = dest_path_str.resolve(&mars_dir);
        if !disk_path.exists() {
            continue;
        }

        // Check for conflict markers
        if crate::merge::file_has_conflict_markers(&disk_path) {
            still_conflicted.push(dest_path_str.clone());
            continue;
        }

        // File has no conflict markers — update lock checksums
        if let Some(item) = lock.items.get_mut(dest_path_str) {
            let new_hash = hash::compute_hash(&disk_path, item.kind)?;
            if new_hash != item.installed_checksum {
                item.installed_checksum = ContentHash::from(new_hash);
                resolved_files.push(dest_path_str.to_string());
            }
        }
    }

    // Write updated lock
    if !resolved_files.is_empty() {
        crate::lock::write(&ctx.project_root, &lock)?;
    }

    if json {
        output::print_json(&serde_json::json!({
            "resolved": resolved_files,
            "still_conflicted": still_conflicted,
        }));
    } else {
        for file in &resolved_files {
            output::print_success(&format!("resolved {file}"));
        }
        for file in &still_conflicted {
            eprintln!("  ! {file} still has conflict markers");
        }
        if resolved_files.is_empty() && still_conflicted.is_empty() {
            output::print_info("no conflicts to resolve");
        }
    }

    if still_conflicted.is_empty() {
        Ok(0)
    } else {
        Ok(1)
    }
}
