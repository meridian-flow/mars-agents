//! `mars list` — show available agents and skills.

use std::path::Path;

use crate::error::MarsError;
use crate::frontmatter;
use crate::hash;
use crate::lock::ItemKind;

use super::output::{self, CatalogEntry, ListEntry};

/// Arguments for `mars list`.
#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Filter by source name.
    #[arg(long)]
    pub source: Option<String>,

    /// Filter by item kind (agents, skills).
    #[arg(long)]
    pub kind: Option<String>,

    /// Show detailed status (source, version, hash check).
    #[arg(long)]
    pub status: bool,
}

/// Run `mars list`.
pub fn run(args: &ListArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(root)?;

    if args.status {
        return run_status(args, root, &lock, json);
    }

    // Default: catalog view (name + description from frontmatter)
    let mut agents = Vec::new();
    let mut skills = Vec::new();

    for (dest_path, item) in &lock.items {
        // Filter by source
        if let Some(ref filter_source) = args.source
            && &item.source != filter_source
        {
            continue;
        }

        // Filter by kind
        if let Some(ref filter_kind) = args.kind {
            let kind_str = match item.kind {
                ItemKind::Agent => "agents",
                ItemKind::Skill => "skills",
            };
            if kind_str != filter_kind && &item.kind.to_string() != filter_kind {
                continue;
            }
        }

        // Read frontmatter for name + description
        let disk_path = root.join(dest_path);
        let content_path = match item.kind {
            ItemKind::Agent => disk_path.clone(),
            ItemKind::Skill => disk_path.join("SKILL.md"),
        };

        let (name, description) = read_name_description(&content_path);

        let entry = CatalogEntry {
            name,
            description,
            kind: item.kind.to_string(),
        };

        match item.kind {
            ItemKind::Agent => agents.push(entry),
            ItemKind::Skill => skills.push(entry),
        }
    }

    agents.sort_by(|a, b| a.name.cmp(&b.name));
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    if json {
        output::print_json(&serde_json::json!({
            "agents": agents,
            "skills": skills,
        }));
    } else {
        output::print_catalog(&agents, &skills, args.kind.as_deref());
    }

    Ok(0)
}

/// Read name and description from a file's frontmatter.
fn read_name_description(path: &Path) -> (String, String) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (path_to_name(path), String::new()),
    };
    match frontmatter::parse(&content) {
        Ok(fm) => {
            let name = fm
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| path_to_name(path));
            let description = fm
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            (name, description)
        }
        Err(_) => (path_to_name(path), String::new()),
    }
}

/// Derive a display name from a file path.
fn path_to_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.parent().and_then(|p| p.file_name()))
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Status view (original table with source, version, hash check).
fn run_status(
    args: &ListArgs,
    root: &Path,
    lock: &crate::lock::LockFile,
    json: bool,
) -> Result<i32, MarsError> {
    let mut entries = Vec::new();

    for (dest_path, item) in &lock.items {
        if let Some(ref filter_source) = args.source
            && &item.source != filter_source
        {
            continue;
        }

        if let Some(ref filter_kind) = args.kind {
            let kind_str = match item.kind {
                ItemKind::Agent => "agents",
                ItemKind::Skill => "skills",
            };
            if kind_str != filter_kind && &item.kind.to_string() != filter_kind {
                continue;
            }
        }

        let disk_path = root.join(dest_path);
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
            source: item.source.to_string(),
            item: dest_path.to_string(),
            kind: item.kind.to_string(),
            version: item.version.clone().unwrap_or_else(|| "-".to_string()),
            status,
        });
    }

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
