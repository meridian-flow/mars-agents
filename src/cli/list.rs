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
pub fn run(args: &ListArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(&ctx.project_root)?;

    let mars_dir = ctx.project_root.join(".mars");
    if args.status {
        return run_status(args, &mars_dir, &lock, json);
    }

    // Default: catalog view (name + description from frontmatter)
    let mut agents = Vec::new();
    let mut skills = Vec::new();

    for (dest_path, item) in lock.flat_items() {
        // Filter by source
        if let Some(ref filter_source) = args.source
            && item.source != *filter_source
        {
            continue;
        }

        // Filter by kind
        if let Some(ref filter_kind) = args.kind {
            let kind_str = match item.kind {
                ItemKind::Agent => "agents",
                ItemKind::Skill => "skills",
                ItemKind::Hook => "hooks",
                ItemKind::McpServer => "mcp",
                ItemKind::BootstrapDoc => "bootstrap",
            };
            if kind_str != filter_kind && &item.kind.to_string() != filter_kind {
                continue;
            }
        }

        // Read frontmatter for name + description from .mars/ canonical store
        let disk_path = dest_path.resolve(&mars_dir);
        let content_path = match item.kind {
            ItemKind::Agent | ItemKind::Hook | ItemKind::McpServer | ItemKind::BootstrapDoc => {
                disk_path.clone()
            }
            ItemKind::Skill => disk_path.join("SKILL.md"),
        };

        let fallback_name = path_to_name(&disk_path);
        let (name, description) = read_name_description(&content_path, &fallback_name);

        let entry = CatalogEntry {
            name,
            description,
            kind: item.kind.to_string(),
        };

        match item.kind {
            ItemKind::Agent => agents.push(entry),
            ItemKind::Skill => skills.push(entry),
            // New kinds not yet shown in the default catalog view — no-op for now.
            ItemKind::Hook | ItemKind::McpServer | ItemKind::BootstrapDoc => {}
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
fn read_name_description(path: &Path, fallback_name: &str) -> (String, String) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (fallback_name.to_string(), String::new()),
    };
    match frontmatter::parse(&content) {
        Ok(fm) => {
            let name = fm
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| fallback_name.to_string());
            let description = fm
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            (name, description)
        }
        Err(_) => (fallback_name.to_string(), String::new()),
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

    for (dest_path, item) in lock.flat_items() {
        if let Some(ref filter_source) = args.source
            && item.source != *filter_source
        {
            continue;
        }

        if let Some(ref filter_kind) = args.kind {
            let kind_str = match item.kind {
                ItemKind::Agent => "agents",
                ItemKind::Skill => "skills",
                ItemKind::Hook => "hooks",
                ItemKind::McpServer => "mcp",
                ItemKind::BootstrapDoc => "bootstrap",
            };
            if kind_str != filter_kind && &item.kind.to_string() != filter_kind {
                continue;
            }
        }

        let disk_path = dest_path.resolve(root);
        let status = if !disk_path.exists() {
            "missing".to_string()
        } else if crate::merge::file_has_conflict_markers(&disk_path) {
            "conflicted".to_string()
        } else {
            let disk_hash = hash::compute_hash(&disk_path, item.kind)?;
            if disk_hash == item.installed_checksum.as_ref() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_name_description_uses_provided_fallback_name() {
        let dir = TempDir::new().unwrap();
        let missing_skill_md = dir.path().join("skills/test-skill/SKILL.md");
        let (name, description) = read_name_description(&missing_skill_md, "test-skill");
        assert_eq!(name, "test-skill");
        assert_eq!(description, "");
    }
}
