//! `mars check [PATH]` — validate a source package before publishing.
//!
//! Scans a directory as a mars source package
//! (`agents/*.md`, `skills/*/SKILL.md`, or a flat root `SKILL.md`)
//! and validates structure, frontmatter, and internal skill dependencies.
//! No config or lock file needed — works on raw source directories.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::discover;
use crate::error::MarsError;
use crate::frontmatter;

use super::output;

/// Arguments for `mars check`.
#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    /// Directory to validate as a source package (default: current directory).
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CheckReport {
    agents: usize,
    skills: usize,
    pub(crate) errors: Vec<String>,
    warnings: Vec<String>,
}

/// Run `mars check`.
pub fn run(args: &CheckArgs, json: bool) -> Result<i32, MarsError> {
    let base = match &args.path {
        Some(p) => {
            if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir()?.join(p)
            }
        }
        None => std::env::current_dir()?,
    };

    if !base.is_dir() {
        return Err(MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("{} is not a directory", base.display()),
        }));
    }

    let report = check_dir(&base)?;

    if json {
        output::print_json(&report);
    } else {
        println!("  {} agents, {} skills", report.agents, report.skills);
        println!();

        if report.errors.is_empty() && report.warnings.is_empty() {
            output::print_success("all checks passed");
        } else {
            for e in &report.errors {
                output::print_error(e);
            }
            for w in &report.warnings {
                output::print_warn(w);
            }
            if !report.errors.is_empty() {
                println!();
                println!("  {} error(s) found", report.errors.len());
            }
        }
    }

    if report.errors.is_empty() {
        Ok(0)
    } else {
        Ok(1)
    }
}

pub(crate) fn check_dir(base: &Path) -> Result<CheckReport, MarsError> {
    let skills_dir = base.join("skills");

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let discovered = discover::discover_resolved_source(base, None)?;

    // ── Validate discovered agents/skills ────────────────────────────
    let mut agent_names: HashMap<String, PathBuf> = HashMap::new();
    let mut agent_skill_refs: Vec<(String, Vec<String>)> = Vec::new();
    let mut skill_names: HashMap<String, PathBuf> = HashMap::new();

    for item in discovered {
        let path = base.join(&item.source_path);
        match item.id.kind {
            crate::lock::ItemKind::Agent => {
                if super::is_symlink(&path) {
                    let name = path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    warnings.push(format!(
                        "skipping symlinked agent `{name}` — source packages should not contain symlinks"
                    ));
                    continue;
                }

                let filename = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();

                match std::fs::read_to_string(&path) {
                    Ok(content) => match frontmatter::parse(&content) {
                        Ok(fm) => {
                            let name = fm
                                .name()
                                .map(str::to_string)
                                .unwrap_or_else(|| filename.clone());

                            if fm.name().is_none() {
                                warnings.push(format!(
                                    "agent `{filename}` has no `name` in frontmatter"
                                ));
                            }

                            if fm.get("description").and_then(|v| v.as_str()).is_none() {
                                warnings.push(format!("agent `{name}` has no `description`"));
                            }

                            if fm.name().is_some() && name != filename {
                                warnings.push(format!(
                                    "agent filename `{filename}.md` doesn't match name `{name}` in frontmatter"
                                ));
                            }

                            if let Some(existing) = agent_names.get(&name) {
                                errors.push(format!(
                                    "duplicate agent name `{name}` in {} and {}",
                                    existing.display(),
                                    path.display()
                                ));
                            } else {
                                agent_names.insert(name.clone(), path.clone());
                            }

                            let skills = fm.skills();
                            if !skills.is_empty() {
                                agent_skill_refs.push((name, skills));
                            }
                        }
                        Err(e) => {
                            errors.push(format!("agent `{filename}` has invalid frontmatter: {e}"));
                        }
                    },
                    Err(e) => {
                        errors.push(format!("cannot read {}: {e}", path.display()));
                    }
                }
            }
            crate::lock::ItemKind::Skill => {
                let (dirname, skill_md, duplicate_path) = if item.source_path
                    == std::path::Path::new(".")
                {
                    let dirname = item.id.name.to_string();
                    (dirname, base.join("SKILL.md"), base.join("SKILL.md"))
                } else {
                    if super::is_symlink(&path) {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or_default();
                        warnings.push(format!(
                            "skipping symlinked skill `{name}` — source packages should not contain symlinks"
                        ));
                        continue;
                    }
                    let dirname = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    (dirname, path.join("SKILL.md"), path.clone())
                };

                match std::fs::read_to_string(&skill_md) {
                    Ok(content) => match frontmatter::parse(&content) {
                        Ok(fm) => {
                            let name = fm
                                .name()
                                .map(str::to_string)
                                .unwrap_or_else(|| dirname.clone());

                            if fm.name().is_none() {
                                warnings.push(format!(
                                    "skill `{dirname}` has no `name` in frontmatter"
                                ));
                            }

                            if fm.get("description").and_then(|v| v.as_str()).is_none() {
                                warnings.push(format!("skill `{name}` has no `description`"));
                            }

                            if fm.name().is_some() && name != dirname {
                                warnings.push(format!(
                                    "skill dirname `{dirname}` doesn't match name `{name}` in frontmatter"
                                ));
                            }

                            if let Some(existing) = skill_names.get(&name) {
                                errors.push(format!(
                                    "duplicate skill name `{name}` in {} and {}",
                                    existing.display(),
                                    duplicate_path.display()
                                ));
                            } else {
                                skill_names.insert(name, duplicate_path);
                            }
                        }
                        Err(e) => {
                            errors.push(format!("skill `{dirname}` has invalid frontmatter: {e}"));
                        }
                    },
                    Err(e) => {
                        errors.push(format!("cannot read {}: {e}", skill_md.display()));
                    }
                }
            }
        }
    }

    // Structural validation for nested skill layout:
    // if skills/* directories exist, each must contain SKILL.md.
    if skills_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&skills_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let dirname = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !path.join("SKILL.md").exists() {
                errors.push(format!("skill `{dirname}` is missing SKILL.md"));
            }
        }
    }

    let agent_count = agent_names.len();
    let skill_count = skill_names.len();

    // ── Empty package check ──────────────────────────────────────────
    if agent_count == 0 && skill_count == 0 {
        errors.push("no agents or skills found — is this a mars source package?".to_string());
    }

    // ── Skill dependency check ───────────────────────────────────────
    let available: HashSet<&str> = skill_names.keys().map(|s| s.as_str()).collect();
    let dependency_skills = dependency_skills_from_lock(base);
    let mut external_deps: HashMap<String, Vec<String>> = HashMap::new();

    for (agent_name, skills) in &agent_skill_refs {
        for skill in skills {
            if !available.contains(skill.as_str()) && !dependency_skills.contains(skill.as_str()) {
                external_deps
                    .entry(skill.clone())
                    .or_default()
                    .push(agent_name.clone());
            }
        }
    }

    if !external_deps.is_empty() {
        let mut sorted: Vec<_> = external_deps.iter().collect();
        sorted.sort_by_key(|(name, _)| name.as_str());
        for (skill, agents) in &sorted {
            warnings.push(format!(
                "external dependency: `{skill}` (referenced by: {})",
                agents.join(", ")
            ));
        }
    }

    // ── Output ───────────────────────────────────────────────────────
    Ok(CheckReport {
        agents: agent_count,
        skills: skill_count,
        errors,
        warnings,
    })
}

fn dependency_skills_from_lock(base: &Path) -> HashSet<String> {
    let Ok(lock) = crate::lock::load(base) else {
        return HashSet::new();
    };

    lock.items
        .values()
        .filter(|item| item.kind == crate::lock::ItemKind::Skill)
        .filter_map(|item| skill_name_from_dest_path(item.dest_path.as_path()))
        .collect()
}

fn skill_name_from_dest_path(dest_path: &Path) -> Option<String> {
    let mut components = dest_path.components();
    let prefix = components.next()?.as_os_str().to_str()?;
    if prefix != "skills" {
        return None;
    }

    components
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::lock::{ItemKind, LockFile, LockedItem};
    use crate::types::{ContentHash, DestPath, SourceName};
    use tempfile::TempDir;

    fn write_agent(path: &Path, filename: &str, skills: &[&str]) {
        let agents = path.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let skills = skills.join(", ");
        std::fs::write(
            agents.join(format!("{filename}.md")),
            format!(
                "---\nname: {filename}\ndescription: test agent\nskills: [{skills}]\n---\n# Agent"
            ),
        )
        .unwrap();
    }

    fn write_lock_skill(path: &Path, skill_name: &str) {
        let mut lock = LockFile::empty();
        let dest_path = DestPath::from(format!("skills/{skill_name}"));
        lock.items.insert(
            dest_path.clone(),
            LockedItem {
                source: SourceName::from("dep-source"),
                kind: ItemKind::Skill,
                version: None,
                source_checksum: ContentHash::from("source-hash"),
                installed_checksum: ContentHash::from("installed-hash"),
                dest_path,
            },
        );
        crate::lock::write(path, &lock).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn check_skips_symlinked_agent() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();

        // Real agent
        std::fs::write(
            agents.join("real.md"),
            "---\nname: real\ndescription: real agent\n---\n# Real",
        )
        .unwrap();

        // Symlinked agent pointing to the real one
        std::os::unix::fs::symlink(agents.join("real.md"), agents.join("linked.md")).unwrap();

        let args = super::CheckArgs {
            path: Some(dir.path().to_path_buf()),
        };
        // Should succeed (the symlink is warned, not errored)
        let code = super::run(&args, true).unwrap();
        // No structural errors — the real agent is valid
        assert_eq!(code, 0);
    }

    #[cfg(unix)]
    #[test]
    fn check_skips_symlinked_skill() {
        let dir = TempDir::new().unwrap();
        let skills = dir.path().join("skills");
        let real_skill = skills.join("real-skill");
        std::fs::create_dir_all(&real_skill).unwrap();
        std::fs::write(
            real_skill.join("SKILL.md"),
            "---\nname: real-skill\ndescription: a skill\n---\n# Skill",
        )
        .unwrap();

        // Symlinked skill dir
        std::os::unix::fs::symlink(&real_skill, skills.join("linked-skill")).unwrap();

        // Also add an agent so the package isn't empty
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("coder.md"),
            "---\nname: coder\ndescription: agent\n---\n# Coder",
        )
        .unwrap();

        let args = super::CheckArgs {
            path: Some(dir.path().to_path_buf()),
        };
        let code = super::run(&args, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn check_accepts_flat_skill_repo() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: flat-skill\ndescription: flat layout\n---\n# Flat skill",
        )
        .unwrap();

        let args = super::CheckArgs {
            path: Some(dir.path().to_path_buf()),
        };
        let code = super::run(&args, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn check_suppresses_warning_for_dependency_provided_skill() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "coder", &["ext-skill"]);
        write_lock_skill(dir.path(), "ext-skill");

        let report = super::check_dir(dir.path()).unwrap();
        let has_external_warning = report
            .warnings
            .iter()
            .any(|w| w.contains("external dependency: `ext-skill`"));

        assert!(
            !has_external_warning,
            "unexpected external dependency warning: {:?}",
            report.warnings
        );
    }

    #[test]
    fn check_warns_for_truly_missing_external_skill() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "coder", &["missing-skill"]);
        write_lock_skill(dir.path(), "some-other-skill");

        let report = super::check_dir(dir.path()).unwrap();
        let has_missing_warning = report
            .warnings
            .iter()
            .any(|w| w.contains("external dependency: `missing-skill`"));

        assert!(
            has_missing_warning,
            "expected missing external dependency warning, got: {:?}",
            report.warnings
        );
    }
}
