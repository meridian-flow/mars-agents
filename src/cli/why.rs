//! `mars why <name>` — explain why an item is installed.

use std::path::Path;

use serde::Serialize;

use crate::error::MarsError;
use crate::lock::ItemKind;

use super::output;

/// Arguments for `mars why`.
#[derive(Debug, clap::Args)]
pub struct WhyArgs {
    /// Item name to explain (e.g., "frontend-design" or "coder").
    pub name: String,
}

#[derive(Debug, Serialize)]
struct WhyResult {
    name: String,
    kind: String,
    source: String,
    version: String,
    dest_path: String,
    required_by: Vec<String>,
}

/// Run `mars why`.
pub fn run(args: &WhyArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(&ctx.project_root)?;

    // Find the item by name (try matching dest_path, name stem, or skill dir name)
    let mut found = None;
    for (dest_path, item) in &lock.items {
        let name_matches =
            dest_path.item_name(item.kind) == args.name || dest_path.as_str() == args.name;

        if name_matches {
            found = Some((dest_path.clone(), item.clone()));
            break;
        }
    }

    let (dest_path, item) = match found {
        Some(f) => f,
        None => {
            return Err(MarsError::Source {
                source_name: "why".to_string(),
                message: format!("item `{}` not found in lock file", args.name),
            });
        }
    };

    // Find which agents reference this item (if it's a skill)
    let mars_dir = ctx.project_root.join(".mars");
    let required_by = if item.kind == ItemKind::Skill {
        let skill_name = dest_path.item_name(ItemKind::Skill);
        find_referencing_agents(&mars_dir, &lock, &skill_name)
    } else {
        Vec::new()
    };

    let result = WhyResult {
        name: args.name.clone(),
        kind: item.kind.to_string(),
        source: item.source.to_string(),
        version: item.version.clone().unwrap_or_else(|| "-".to_string()),
        dest_path: dest_path.to_string(),
        required_by: required_by.clone(),
    };

    if json {
        output::print_json(&result);
    } else {
        println!("{} ({})", args.name, item.kind);
        println!(
            "  provided by: {}@{}",
            item.source,
            item.version.as_deref().unwrap_or("-")
        );
        println!("  installed at: {dest_path}");
        if required_by.is_empty() {
            println!("  required by: (no dependents)");
        } else {
            println!("  required by:");
            for agent in &required_by {
                println!("    {agent}");
            }
        }
    }

    Ok(0)
}

/// Find agents that reference a skill name in their frontmatter.
fn find_referencing_agents(
    root: &Path,
    lock: &crate::lock::LockFile,
    skill_name: &str,
) -> Vec<String> {
    let mut refs = Vec::new();

    for (dest_path, item) in &lock.items {
        if item.kind != ItemKind::Agent {
            continue;
        }

        let agent_path = dest_path.resolve(root);
        if let Ok(skills) = crate::validate::parse_agent_skills(&agent_path)
            && skills.iter().any(|s| s == skill_name)
        {
            refs.push(dest_path.to_string());
        }
    }

    refs.sort();
    refs
}
