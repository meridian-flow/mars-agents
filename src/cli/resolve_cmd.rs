//! `mars resolve` — mark conflicts as resolved after user fixes them.

use std::path::{Path, PathBuf};

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
pub fn run(args: &ResolveArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let mut lock = crate::lock::load(root)?;
    let mut resolved_files = Vec::new();
    let mut still_conflicted = Vec::new();

    let items_to_check: Vec<DestPath> = if let Some(file) = &args.file {
        // Check specific file
        let rel = DestPath::from(file.as_path());
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
        let disk_path = root.join(dest_path_str);
        if !disk_path.exists() {
            continue;
        }

        // Check for conflict markers
        if has_conflict_markers(&disk_path)? {
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
        crate::lock::write(root, &lock)?;
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

/// Check if a file contains git conflict markers.
fn has_conflict_markers(path: &Path) -> Result<bool, MarsError> {
    let content = std::fs::read_to_string(path)?;
    Ok(content.contains("<<<<<<<") && content.contains(">>>>>>>"))
}
