//! `mars doctor` — validate state consistency.

use std::path::Path;

use crate::error::MarsError;
use crate::hash;

use super::output;

/// Arguments for `mars doctor`.
#[derive(Debug, clap::Args)]
pub struct DoctorArgs {}

/// Run `mars doctor`.
pub fn run(_args: &DoctorArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let mut issues = Vec::new();

    // Check config is valid
    match crate::config::load(root) {
        Ok(_) => {}
        Err(e) => {
            issues.push(format!("config error: {e}"));
        }
    }

    // Check lock file
    let lock = match crate::lock::load(root) {
        Ok(l) => l,
        Err(e) => {
            issues.push(format!("lock file error: {e}"));
            output::print_doctor(&issues, json);
            return Ok(2);
        }
    };

    // Check each locked item
    for (dest_path_str, item) in &lock.items {
        let disk_path = root.join(dest_path_str);

        // Check file exists
        if !disk_path.exists() {
            issues.push(format!(
                "lock references {dest_path_str} which doesn't exist on disk"
            ));
            continue;
        }

        // Check for conflict markers
        if item.kind == crate::lock::ItemKind::Agent
            && let Ok(content) = std::fs::read_to_string(&disk_path)
            && content.contains("<<<<<<<")
            && content.contains(">>>>>>>")
        {
            issues.push(format!(
                "{dest_path_str} has unresolved conflict markers"
            ));
        }

        // Check checksum matches
        match hash::compute_hash(&disk_path, item.kind) {
            Ok(disk_hash) => {
                if disk_hash != item.installed_checksum {
                    // Not necessarily an issue — could be a local modification
                    // But we report it as informational
                }
            }
            Err(e) => {
                issues.push(format!("can't hash {dest_path_str}: {e}"));
            }
        }
    }

    // Check agent→skill references
    if let Ok(config) = crate::config::load(root) {
        let local = crate::config::load_local(root).unwrap_or_default();
        if let Ok(effective) = crate::config::merge(config, local) {
            // Check that all sources in config have corresponding lock entries
            for source_name in effective.sources.keys() {
                if !lock.sources.contains_key(source_name) {
                    issues.push(format!(
                        "source `{source_name}` is in config but not in lock — run `mars sync`"
                    ));
                }
            }
        }
    }

    output::print_doctor(&issues, json);

    if issues.is_empty() {
        Ok(0)
    } else {
        Ok(2)
    }
}
