//! `mars check [PATH]` — validate a source package before publishing.
//!
//! Scans a directory as a mars source package (agents/*.md, skills/*/SKILL.md)
//! and validates structure, frontmatter, and internal skill dependencies.
//! No config or lock file needed — works on raw source directories.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::Serialize;

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
struct CheckReport {
    agents: usize,
    skills: usize,
    errors: Vec<String>,
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

    let agents_dir = base.join("agents");
    let skills_dir = base.join("skills");

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ── Discover agents ──────────────────────────────────────────────
    let mut agent_names: HashMap<String, PathBuf> = HashMap::new();
    let mut agent_skill_refs: Vec<(String, Vec<String>)> = Vec::new();

    if agents_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&agents_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().and_then(|x| x.to_str()) == Some("md") && e.path().is_file()
            })
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
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

                        // Check name field exists
                        if fm.name().is_none() {
                            warnings.push(format!("agent `{filename}` has no `name` in frontmatter"));
                        }

                        // Check description
                        if fm.get("description").and_then(|v| v.as_str()).is_none() {
                            warnings.push(format!("agent `{name}` has no `description`"));
                        }

                        // Check name/filename match
                        if fm.name().is_some() && name != filename {
                            warnings.push(format!(
                                "agent filename `{filename}.md` doesn't match name `{name}` in frontmatter"
                            ));
                        }

                        // Check for duplicate names
                        if let Some(existing) = agent_names.get(&name) {
                            errors.push(format!(
                                "duplicate agent name `{name}` in {} and {}",
                                existing.display(),
                                path.display()
                            ));
                        } else {
                            agent_names.insert(name.clone(), path.clone());
                        }

                        // Collect skill references
                        let skills = fm.skills();
                        if !skills.is_empty() {
                            agent_skill_refs.push((name, skills));
                        }
                    }
                    Err(e) => {
                        errors.push(format!(
                            "agent `{filename}` has invalid frontmatter: {e}"
                        ));
                    }
                },
                Err(e) => {
                    errors.push(format!("cannot read {}: {e}", path.display()));
                }
            }
        }
    }

    // ── Discover skills ──────────────────────────────────────────────
    let mut skill_names: HashMap<String, PathBuf> = HashMap::new();

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
                .unwrap_or_default()
                .to_string();

            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                errors.push(format!(
                    "skill `{dirname}` is missing SKILL.md"
                ));
                continue;
            }

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
                                path.display()
                            ));
                        } else {
                            skill_names.insert(name, path);
                        }
                    }
                    Err(e) => {
                        errors.push(format!(
                            "skill `{dirname}` has invalid frontmatter: {e}"
                        ));
                    }
                },
                Err(e) => {
                    errors.push(format!("cannot read {}: {e}", skill_md.display()));
                }
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
    let mut external_deps: HashMap<String, Vec<String>> = HashMap::new();

    for (agent_name, skills) in &agent_skill_refs {
        for skill in skills {
            if !available.contains(skill.as_str()) {
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
    let report = CheckReport {
        agents: agent_count,
        skills: skill_count,
        errors: errors.clone(),
        warnings: warnings.clone(),
    };

    if json {
        output::print_json(&report);
    } else {
        println!("  {} agents, {} skills", agent_count, skill_count);
        println!();

        if errors.is_empty() && warnings.is_empty() {
            output::print_success("all checks passed");
        } else {
            for e in &errors {
                output::print_error(e);
            }
            for w in &warnings {
                output::print_warn(w);
            }
            if !errors.is_empty() {
                println!();
                println!("  {} error(s) found", errors.len());
            }
        }
    }

    if errors.is_empty() { Ok(0) } else { Ok(1) }
}
