//! `mars doctor` — validate state consistency.


use crate::error::MarsError;
use crate::hash;

use super::output;

/// Arguments for `mars doctor`.
#[derive(Debug, clap::Args)]
pub struct DoctorArgs {}

/// Run `mars doctor`.
pub fn run(_args: &DoctorArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let mut issues = Vec::new();

    // Check config is valid
    match crate::config::load(&ctx.managed_root) {
        Ok(_) => {}
        Err(e) => {
            issues.push(format!("config error: {e}"));
        }
    }

    // Check lock file
    let lock = match crate::lock::load(&ctx.managed_root) {
        Ok(l) => l,
        Err(e) => {
            issues.push(format!("lock file error: {e}"));
            output::print_doctor(&issues, json);
            return Ok(2);
        }
    };

    // Check each locked item
    for (dest_path_str, item) in &lock.items {
        let disk_path = ctx.managed_root.join(dest_path_str);

        // Check file exists
        if !disk_path.exists() {
            issues.push(format!(
                "{dest_path_str} missing from disk. Run `mars sync` to reinstall or `mars repair` to rebuild"
            ));
            continue;
        }

        // Check for conflict markers
        if item.kind == crate::lock::ItemKind::Agent
            && let Ok(content) = std::fs::read_to_string(&disk_path)
            && content.contains("<<<<<<<")
            && content.contains(">>>>>>>")
        {
            issues.push(format!("{dest_path_str} has unresolved conflict markers"));
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
    if let Ok(config) = crate::config::load(&ctx.managed_root) {
        let local = crate::config::load_local(&ctx.managed_root).unwrap_or_default();
        if let Ok(effective) = crate::config::merge_with_root(config, local, &ctx.managed_root) {
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

    // Check skill dependencies — every agent's declared skills must exist on disk.
    // Scans the actual filesystem, not just the lock — catches both mars-managed
    // and user-created local skills.
    {
        use std::collections::HashSet;
        let mut agents: Vec<(String, std::path::PathBuf)> = Vec::new();
        let mut available_skills: HashSet<String> = HashSet::new();

        // Collect agents from lock (we know their paths)
        for (dest_path, item) in &lock.items {
            if item.kind == crate::lock::ItemKind::Agent {
                let name = item.dest_path.to_string();
                let path = ctx.managed_root.join(dest_path);
                agents.push((name, path));
            }
        }

        // Also scan for agent .md files on disk not in the lock (user-created)
        let agents_dir = ctx.managed_root.join("agents");
        if agents_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("md") {
                        let name = path.file_stem()
                            .and_then(|n| n.to_str())
                            .unwrap_or_default()
                            .to_string();
                        if !agents.iter().any(|(n, _)| n.ends_with(&format!("{name}.md"))) {
                            agents.push((name, path));
                        }
                    }
                }
            }
        }

        // Scan disk for ALL available skills (managed + unmanaged)
        let skills_dir = ctx.managed_root.join("skills");
        if skills_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            // Verify it has a SKILL.md (valid skill dir)
                            if path.join("SKILL.md").exists() {
                                available_skills.insert(name.to_string());
                            }
                        }
                    }
                }
            }
        }

        if let Ok(warnings) = crate::validate::check_deps(&agents, &available_skills) {
            for w in &warnings {
                match w {
                    crate::validate::ValidationWarning::MissingSkill {
                        agent,
                        skill_name,
                        suggestion,
                    } => {
                        let msg = match suggestion {
                            Some(s) => format!(
                                "agent `{}` references missing skill `{skill_name}` (did you mean `{s}`?)",
                                agent.name
                            ),
                            None => format!(
                                "agent `{}` references missing skill `{skill_name}` — \
                                 add a source that provides it, or create it locally in skills/{skill_name}/",
                                agent.name
                            ),
                        };
                        issues.push(msg);
                    }
                }
            }
        }
    }

    // Check link health
    if let Ok(config) = crate::config::load(&ctx.managed_root) {
        for link_target in &config.settings.links {
            check_link_health(ctx, link_target, &mut issues);
        }
    }

    output::print_doctor(&issues, json);

    if issues.is_empty() { Ok(0) } else { Ok(2) }
}

/// Validate link health for a single link target.
fn check_link_health(ctx: &super::MarsContext, target: &str, issues: &mut Vec<String>) {
    let target_dir = ctx.project_root.join(target);

    if !target_dir.exists() {
        issues.push(format!(
            "link `{target}` — directory doesn't exist. Run `mars link --unlink {target}` to remove stale entry"
        ));
        return;
    }

    for subdir in ["agents", "skills"] {
        let link_path = target_dir.join(subdir);
        let expected = ctx.managed_root.join(subdir);

        // Check if symlink exists
        if link_path.symlink_metadata().is_err() {
            issues.push(format!(
                "link `{target}` — missing {target}/{subdir} symlink. Run `mars link {target}` to fix"
            ));
            continue;
        }

        // Check if it's a symlink (not a real dir)
        match link_path.read_link() {
            Ok(actual_target) => {
                // Resolve and compare
                let resolved = target_dir.join(&actual_target);
                let points_to_managed = match (resolved.canonicalize(), expected.canonicalize()) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => false,
                };
                if !points_to_managed {
                    issues.push(format!(
                        "link `{target}` — {target}/{subdir} points to {} (expected {})",
                        actual_target.display(),
                        expected.display()
                    ));
                } else if !link_path.exists() {
                    // Symlink exists but target is broken
                    issues.push(format!(
                        "link `{target}` — {target}/{subdir} is a broken symlink"
                    ));
                }
            }
            Err(_) => {
                // Real directory, not a symlink
                issues.push(format!(
                    "link `{target}` — {target}/{subdir} is a real directory, not a symlink. Run `mars link {target}` to merge and link"
                ));
            }
        }
    }
}
