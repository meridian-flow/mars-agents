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
    let config = match crate::config::load(&ctx.project_root) {
        Ok(config) => Some(config),
        Err(e) => {
            errors.push(format!("config error: {e}"));
            None
        }
    };

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
    for (dest_path_str, item) in lock.flat_items() {
        let disk_path = dest_path_str.resolve(&mars_dir);

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
    if let Some(config) = &config {
        let local = crate::config::load_local(&ctx.project_root).unwrap_or_default();
        if let Ok((effective, _diagnostics)) =
            crate::config::merge_with_root(config.clone(), local, &ctx.project_root)
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

    // Check managed targets (.agents/, .claude/, etc.) against lock checksums.
    if let Some(config) = &config {
        let target_divergence_count = check_target_divergence(
            &ctx.project_root,
            &lock,
            &config.settings.managed_targets(),
            &mut warnings,
        );
        if target_divergence_count > 0 {
            warnings.push(
                "target divergence detected; run `mars sync --force` to reset modified files or `mars repair` to restore missing files".to_string(),
            );
        }
    }

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

/// Check managed target directories against lockfile-installed checksums.
///
/// Returns the number of divergent or missing target items found.
fn check_target_divergence(
    project_root: &std::path::Path,
    lock: &crate::lock::LockFile,
    targets: &[String],
    warnings: &mut Vec<String>,
) -> usize {
    let mut divergence_count = 0;

    for target_name in targets {
        for (dest_path, item) in lock.flat_items() {
            let relative_path = std::path::Path::new(target_name).join(dest_path.as_str());
            let target_path = project_root.join(&relative_path);

            if !target_path.exists() && target_path.symlink_metadata().is_err() {
                warnings.push(format!(
                    "missing in target: {}/{}",
                    target_name,
                    dest_path.as_str()
                ));
                divergence_count += 1;
                continue;
            }

            match hash::compute_hash(&target_path, item.kind) {
                Ok(target_hash) => {
                    if target_hash != item.installed_checksum {
                        warnings.push(format!(
                            "divergent in target: {}/{} (local modifications)",
                            target_name,
                            dest_path.as_str()
                        ));
                        divergence_count += 1;
                    }
                }
                Err(e) => {
                    warnings.push(format!(
                        "divergent in target: {}/{} (local modifications; failed to hash: {e})",
                        target_name,
                        dest_path.as_str()
                    ));
                    divergence_count += 1;
                }
            }
        }
    }

    divergence_count
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::check_target_divergence;

    /// Returns (key, LockedItemV2) for insertion into LockFile.items.
    fn make_locked_agent(
        name: &str,
        dest_path: &str,
        expected_content: &str,
    ) -> (String, crate::lock::LockedItemV2) {
        let expected_hash = crate::hash::hash_bytes(expected_content.as_bytes());
        let key = format!("agent/{name}");
        let item = crate::lock::LockedItemV2 {
            source: "test-source".into(),
            kind: crate::lock::ItemKind::Agent,
            version: None,
            source_checksum: expected_hash.clone().into(),
            outputs: vec![crate::lock::OutputRecord {
                target_root: ".mars".to_string(),
                dest_path: dest_path.into(),
                installed_checksum: expected_hash.into(),
            }],
        };
        (key, item)
    }

    #[test]
    fn check_target_divergence_warns_for_missing_and_modified_items() {
        let temp = TempDir::new().expect("create temp dir");
        let root = temp.path();

        let mut lock = crate::lock::LockFile::empty();
        let (k, v) = make_locked_agent("missing", "agents/missing.md", "expected missing");
        lock.items.insert(k, v);
        let (k, v) = make_locked_agent("modified", "agents/modified.md", "expected content");
        lock.items.insert(k, v);

        fs::create_dir_all(root.join(".agents/agents")).expect("create target dir");
        fs::write(root.join(".agents/agents/modified.md"), "local edits")
            .expect("write modified file");

        let mut warnings = Vec::new();
        let divergences =
            check_target_divergence(root, &lock, &[".agents".to_string()], &mut warnings);

        assert_eq!(divergences, 2);
        assert!(
            warnings
                .iter()
                .any(|w| w == "missing in target: .agents/agents/missing.md")
        );
        assert!(
            warnings
                .iter()
                .any(|w| w
                    == "divergent in target: .agents/agents/modified.md (local modifications)")
        );
    }

    #[test]
    fn check_target_divergence_checks_every_managed_target() {
        let temp = TempDir::new().expect("create temp dir");
        let root = temp.path();

        let mut lock = crate::lock::LockFile::empty();
        let (k, v) = make_locked_agent("test", "agents/test.md", "expected content");
        lock.items.insert(k, v);

        fs::create_dir_all(root.join(".agents/agents")).expect("create .agents tree");
        fs::write(root.join(".agents/agents/test.md"), "expected content")
            .expect("write matching file");

        let mut warnings = Vec::new();
        let divergences = check_target_divergence(
            root,
            &lock,
            &[".agents".to_string(), ".claude".to_string()],
            &mut warnings,
        );

        assert_eq!(divergences, 1);
        assert!(
            warnings
                .iter()
                .any(|w| w == "missing in target: .claude/agents/test.md")
        );
    }
}
