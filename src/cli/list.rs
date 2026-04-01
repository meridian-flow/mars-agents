//! `mars list` — show installed items with status.

use std::path::Path;

use crate::error::MarsError;
use crate::hash;

use super::output::{self, ListEntry};

/// Arguments for `mars list`.
#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Filter by source name.
    #[arg(long)]
    pub source: Option<String>,

    /// Filter by item kind (agents, skills).
    #[arg(long)]
    pub kind: Option<String>,
}

/// Run `mars list`.
pub fn run(args: &ListArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(root)?;

    let mut entries = Vec::new();

    for (dest_path_str, item) in &lock.items {
        // Filter by source
        if let Some(ref filter_source) = args.source
            && &item.source != filter_source
        {
            continue;
        }

        // Filter by kind
        if let Some(ref filter_kind) = args.kind {
            let kind_str = match item.kind {
                crate::lock::ItemKind::Agent => "agents",
                crate::lock::ItemKind::Skill => "skills",
            };
            if kind_str != filter_kind && &item.kind.to_string() != filter_kind {
                continue;
            }
        }

        // Compute disk status
        let disk_path = root.join(dest_path_str);
        let status = if !disk_path.exists() {
            "missing".to_string()
        } else if has_conflict_markers(&disk_path) {
            "conflicted".to_string()
        } else {
            let disk_hash = hash::compute_hash(&disk_path, item.kind)?;
            if disk_hash == item.installed_checksum {
                "ok".to_string()
            } else {
                "modified".to_string()
            }
        };

        entries.push(ListEntry {
            source: item.source.clone(),
            item: dest_path_str.clone(),
            kind: item.kind.to_string(),
            version: item.version.clone().unwrap_or_else(|| "-".to_string()),
            status,
        });
    }

    // Sort by source, then item
    entries.sort_by(|a, b| (&a.source, &a.item).cmp(&(&b.source, &b.item)));

    output::print_list(&entries, json);

    Ok(0)
}

/// Quick check for conflict markers.
fn has_conflict_markers(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| content.contains("<<<<<<<") && content.contains(">>>>>>>"))
        .unwrap_or(false)
}
