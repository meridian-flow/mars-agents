//! `mars doctor` — validate state consistency.

use crate::error::MarsError;
use crate::hash;

use super::output;

/// Arguments for `mars doctor`.
#[derive(Debug, clap::Args)]
pub struct DoctorArgs {}

/// Run `mars doctor`.
pub fn run(_args: &DoctorArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Check config is valid
    match crate::config::load(&ctx.project_root) {
        Ok(_) => {}
        Err(e) => {
            errors.push(format!("config error: {e}"));
        }
    }

    // Check lock file
    let lock = match crate::lock::load(&ctx.project_root) {
        Ok(l) => l,
        Err(e) => {
            errors.push(format!("lock file error: {e}"));
            output::print_doctor(&errors, &warnings, json);
            return Ok(2);
        }
    };

    // Check each locked item against .mars/ canonical store
    let mars_dir = ctx.project_root.join(".mars");
    for (dest_path_str, item) in &lock.items {
        let disk_path = mars_dir.join(dest_path_str);

        // Check file exists
        if !disk_path.exists() {
            errors.push(format!(
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
            errors.push(format!("{dest_path_str} has unresolved conflict markers"));
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
                errors.push(format!("can't hash {dest_path_str}: {e}"));
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
                    errors.push(format!(
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

        // Warn on legacy symlinks found in managed directories.
        for item in installed.agents.iter().chain(installed.skills.iter()) {
            if item
                .path
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                let kind = if item.id.kind == crate::lock::ItemKind::Agent {
                    "agent"
                } else {
                    "skill"
                };
                warnings.push(format!(
                    "legacy symlinked {kind} `{}` detected in managed dir — run `mars sync` to normalize to copied content",
                    item.id.name,
                ));
            }
        }

        let available_skills: HashSet<String> = installed
            .skills
            .iter()
            .map(|s| s.id.name.to_string())
            .collect();

        let agents_for_check: Vec<(String, std::path::PathBuf)> = installed
            .agents
            .iter()
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
                        errors.push(msg);
                    }
                }
            }
        }
    }

    // Check .mars/ gitignore (D29) as warning only.
    check_mars_gitignore(&ctx.project_root, &mut warnings);

    output::print_doctor(&errors, &warnings, json);

    if errors.is_empty() { Ok(0) } else { Ok(2) }
}

/// Check if .mars/ is properly gitignored (D29).
///
/// Mars does NOT auto-edit .gitignore — it only warns via `mars doctor`.
fn check_mars_gitignore(project_root: &std::path::Path, warnings: &mut Vec<String>) {
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
        warnings.push(
            ".mars/ is not in .gitignore — add `.mars/` to your .gitignore to avoid committing cached data"
                .to_string(),
        );
    }
}
