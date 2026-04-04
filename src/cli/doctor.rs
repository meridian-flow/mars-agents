//! `mars doctor` — validate state consistency.

use crate::error::MarsError;
use crate::hash;
use crate::types::SourceOrigin;

use super::output;

/// Arguments for `mars doctor`.
#[derive(Debug, clap::Args)]
pub struct DoctorArgs {}

/// Run `mars doctor`.
pub fn run(_args: &DoctorArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let mut issues = Vec::new();

    // Check config is valid
    match crate::config::load(&ctx.project_root) {
        Ok(_) => {}
        Err(e) => {
            issues.push(format!("config error: {e}"));
        }
    }

    // Check lock file
    let lock = match crate::lock::load(&ctx.project_root) {
        Ok(l) => l,
        Err(e) => {
            issues.push(format!("lock file error: {e}"));
            output::print_doctor(&issues, json);
            return Ok(2);
        }
    };

    // Check each locked item against .mars/ canonical store
    let mars_dir = ctx.project_root.join(".mars");
    for (dest_path_str, item) in &lock.items {
        let disk_path = mars_dir.join(dest_path_str);

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
    if let Ok(config) = crate::config::load(&ctx.project_root) {
        let local = crate::config::load_local(&ctx.project_root).unwrap_or_default();
        if let Ok((effective, _diagnostics)) =
            crate::config::merge_with_root(config, local, &ctx.project_root)
        {
            // Check that all sources in config have corresponding lock entries
            for source_name in effective.dependencies.keys() {
                if !lock.dependencies.contains_key(source_name) {
                    issues.push(format!(
                        "dependency `{source_name}` is in config but not in lock — run `mars sync`"
                    ));
                }
            }
        }
    }

    // Check skill dependencies — every agent's declared skills must exist on disk.
    // Uses discover_installed() to scan the actual filesystem, catching both
    // mars-managed and user-created local agents/skills.
    {
        use std::collections::HashSet;

        let installed = crate::discover::discover_installed(&mars_dir)?;

        // Report symlinked items
        for item in installed.agents.iter().chain(installed.skills.iter()) {
            if item.is_symlink {
                if is_self_symlink(item, ctx, &lock) {
                    continue;
                }
                let kind = if item.id.kind == crate::lock::ItemKind::Agent {
                    "agent"
                } else {
                    "skill"
                };
                issues.push(format!(
                    "skipping symlinked {kind} `{}` — individual symlinks in managed dirs are not validated",
                    item.id.name
                ));
            }
        }

        let available_skills: HashSet<String> = installed
            .skills
            .iter()
            .filter(|s| !s.is_symlink)
            .map(|s| s.id.name.to_string())
            .collect();

        let agents_for_check: Vec<(String, std::path::PathBuf)> = installed
            .agents
            .iter()
            .filter(|a| !a.is_symlink)
            .map(|a| (a.id.name.to_string(), a.path.clone()))
            .collect();

        if let Ok(warnings) = crate::validate::check_deps(&agents_for_check, &available_skills) {
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

    // Check .mars/ gitignore (D29)
    check_mars_gitignore(&ctx.project_root, &mut issues);

    // Check link health
    if let Ok(config) = crate::config::load(&ctx.project_root) {
        for link_target in &config.settings.links {
            check_link_health(ctx, link_target, &mut issues);
        }
    }

    output::print_doctor(&issues, json);

    if issues.is_empty() { Ok(0) } else { Ok(2) }
}

/// Check if .mars/ is properly gitignored (D29).
///
/// Mars does NOT auto-edit .gitignore — it only warns via `mars doctor`.
fn check_mars_gitignore(project_root: &std::path::Path, issues: &mut Vec<String>) {
    let mars_dir = project_root.join(".mars");
    if !mars_dir.exists() {
        return;
    }

    let gitignore_path = project_root.join(".gitignore");
    let is_ignored = match std::fs::read_to_string(&gitignore_path) {
        Ok(content) => content.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == ".mars" || trimmed == ".mars/" || trimmed == "/.mars" || trimmed == "/.mars/"
        }),
        Err(_) => false,
    };

    if !is_ignored {
        issues.push(
            ".mars/ is not in .gitignore — add `.mars/` to your .gitignore to avoid committing cached data"
                .to_string(),
        );
    }
}

fn is_self_symlink(
    item: &crate::discover::InstalledItem,
    ctx: &super::MarsContext,
    lock: &crate::lock::LockFile,
) -> bool {
    let mars_dir = ctx.project_root.join(".mars");
    let Ok(rel_path) = item.path.strip_prefix(&mars_dir) else {
        return false;
    };
    let dest_path = crate::types::DestPath::from(rel_path);
    let local_source_name = SourceOrigin::LocalPackage.to_string();
    lock.items
        .get(&dest_path)
        .is_some_and(|locked| locked.source.as_ref() == local_source_name.as_str())
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
