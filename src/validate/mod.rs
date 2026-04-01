use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind};

/// Warning from dependency validation.
///
/// Agents declare `skills: [X, Y]` in YAML frontmatter. After resolution,
/// every referenced skill must exist somewhere in the target state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationWarning {
    /// An agent references a skill that doesn't exist in target state.
    MissingSkill {
        agent: ItemId,
        skill_name: String,
        /// Fuzzy match suggestion: "did you mean X?"
        suggestion: Option<String>,
    },
    /// A skill is installed but no agent references it.
    OrphanedSkill { skill: ItemId },
}

/// Minimal struct for parsing agent YAML frontmatter.
#[derive(Deserialize)]
struct AgentFrontmatter {
    #[serde(default)]
    skills: Option<Vec<String>>,
}

/// Parse YAML frontmatter from an agent .md file.
///
/// Returns the `skills` list, or empty vec if no frontmatter, no skills
/// field, or malformed YAML. Only reads the frontmatter block between
/// `---` delimiters, not the full markdown body.
pub fn parse_agent_skills(agent_path: &Path) -> Result<Vec<String>, MarsError> {
    let content = std::fs::read_to_string(agent_path)?;
    Ok(extract_skills_from_content(&content))
}

/// Extract skills list from markdown content with YAML frontmatter.
///
/// Defensive: returns empty vec on any parse failure.
fn extract_skills_from_content(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Vec::new();
    }

    // Skip past the opening `---` and any immediate newline
    let after_opening = &trimmed[3..];
    let rest = after_opening.trim_start_matches(['\r', '\n']);

    // Find closing `---` on its own line
    let yaml_block = match rest.find("\n---") {
        Some(pos) => &rest[..pos],
        None => {
            // Handle edge case: single-line frontmatter like "---\nkey: val\n---"
            // where rest might be "key: val\n---" — already handled above.
            // If truly no closing delimiter, no valid frontmatter.
            return Vec::new();
        }
    };

    match serde_yaml::from_str::<AgentFrontmatter>(yaml_block) {
        Ok(fm) => fm.skills.unwrap_or_default(),
        Err(_) => Vec::new(), // Malformed YAML → empty skills list
    }
}

/// Check that agent→skill references resolve.
///
/// Reads YAML frontmatter from each agent .md file to extract `skills: [...]`.
/// Checks each referenced skill name exists in `available_skills`.
/// Also detects orphaned skills (installed but not referenced by any agent).
///
/// Returns warnings, not errors — a missing skill doesn't prevent sync.
pub fn check_deps(
    agents: &[(String, PathBuf)],
    available_skills: &HashSet<String>,
) -> Result<Vec<ValidationWarning>, MarsError> {
    let mut warnings = Vec::new();
    let mut referenced_skills: HashSet<String> = HashSet::new();

    for (agent_name, agent_path) in agents {
        // Defensive: if we can't read/parse the file, treat as no skills
        let skills = parse_agent_skills(agent_path).unwrap_or_default();

        for skill_name in skills {
            referenced_skills.insert(skill_name.clone());

            if !available_skills.contains(&skill_name) {
                let suggestion = find_suggestion(&skill_name, available_skills);
                warnings.push(ValidationWarning::MissingSkill {
                    agent: ItemId {
                        kind: ItemKind::Agent,
                        name: agent_name.clone(),
                    },
                    skill_name,
                    suggestion,
                });
            }
        }
    }

    // Check for orphaned skills (installed but not referenced)
    let mut orphaned: Vec<&String> = available_skills
        .iter()
        .filter(|s| !referenced_skills.contains(*s))
        .collect();
    orphaned.sort(); // Deterministic order

    for skill_name in orphaned {
        warnings.push(ValidationWarning::OrphanedSkill {
            skill: ItemId {
                kind: ItemKind::Skill,
                name: skill_name.clone(),
            },
        });
    }

    Ok(warnings)
}

/// Find a suggestion for a missing skill using substring matching.
///
/// Checks if any available skill name contains the missing name as a
/// substring or vice versa. No edit distance library needed for v1.
fn find_suggestion(missing: &str, available: &HashSet<String>) -> Option<String> {
    let missing_lower = missing.to_lowercase();

    // Sort for deterministic suggestion when multiple match
    let mut candidates: Vec<&String> = available.iter().collect();
    candidates.sort();

    for name in candidates {
        let name_lower = name.to_lowercase();
        if name_lower.contains(&missing_lower) || missing_lower.contains(&name_lower) {
            return Some(name.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── Frontmatter parsing tests ───────────────────────────────────

    #[test]
    fn parse_valid_frontmatter_with_skills() {
        let content = "---\nskills:\n  - planning\n  - review\n---\n\n# Agent body\n";
        let skills = extract_skills_from_content(content);
        assert_eq!(skills, vec!["planning", "review"]);
    }

    #[test]
    fn parse_frontmatter_inline_skills() {
        let content = "---\nskills: [alpha, beta]\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert_eq!(skills, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_no_frontmatter() {
        let content = "# Just markdown\n\nNo frontmatter here.\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_frontmatter_without_skills_field() {
        let content = "---\nmodel: opus\napproval: auto\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_frontmatter_empty_skills() {
        let content = "---\nskills: []\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_empty_frontmatter() {
        let content = "---\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_malformed_yaml_returns_empty() {
        let content = "---\nskills: [[[invalid yaml\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_no_closing_delimiter() {
        let content = "---\nskills: [a, b]\n";
        let skills = extract_skills_from_content(content);
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_empty_content() {
        let skills = extract_skills_from_content("");
        assert!(skills.is_empty());
    }

    #[test]
    fn parse_frontmatter_with_extra_fields() {
        let content = "---\nmodel: opus\nskills:\n  - planning\n  - review\napproval: auto\n---\n\n# Agent\n";
        let skills = extract_skills_from_content(content);
        assert_eq!(skills, vec!["planning", "review"]);
    }

    #[test]
    fn parse_agent_skills_from_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agent.md");
        fs::write(&path, "---\nskills: [a, b]\n---\n\n# Agent\n").unwrap();

        let skills = parse_agent_skills(&path).unwrap();
        assert_eq!(skills, vec!["a", "b"]);
    }

    #[test]
    fn parse_agent_skills_file_not_found() {
        let result = parse_agent_skills(Path::new("/nonexistent/agent.md"));
        assert!(result.is_err());
    }

    // ── Validation tests ────────────────────────────────────────────

    fn write_agent(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn all_skills_present_no_warnings() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(
            dir.path(),
            "coder",
            "---\nskills: [planning, review]\n---\n# Coder\n",
        );

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["planning", "review"].iter().map(|s| s.to_string()).collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn missing_skill_produces_warning() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(
            dir.path(),
            "coder",
            "---\nskills: [missing-skill]\n---\n# Coder\n",
        );

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = HashSet::new();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            ValidationWarning::MissingSkill {
                agent,
                skill_name,
                suggestion,
            } => {
                assert_eq!(agent.name, "coder");
                assert_eq!(agent.kind, ItemKind::Agent);
                assert_eq!(skill_name, "missing-skill");
                assert!(suggestion.is_none());
            }
            other => panic!("expected MissingSkill, got {other:?}"),
        }
    }

    #[test]
    fn orphaned_skill_produces_warning() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(dir.path(), "coder", "---\nskills: []\n---\n# Coder\n");

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["unused-skill"].iter().map(|s| s.to_string()).collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert_eq!(warnings.len(), 1);
        match &warnings[0] {
            ValidationWarning::OrphanedSkill { skill } => {
                assert_eq!(skill.name, "unused-skill");
                assert_eq!(skill.kind, ItemKind::Skill);
            }
            other => panic!("expected OrphanedSkill, got {other:?}"),
        }
    }

    #[test]
    fn agent_with_no_frontmatter_no_warnings() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(dir.path(), "simple", "# Simple agent\n\nNo frontmatter.\n");

        let agents = vec![("simple".to_string(), p)];
        let skills: HashSet<String> = HashSet::new();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn agent_with_malformed_yaml_no_crash() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(
            dir.path(),
            "broken",
            "---\n{{invalid: yaml[[\n---\n# Broken\n",
        );

        let agents = vec![("broken".to_string(), p)];
        let skills: HashSet<String> = HashSet::new();

        let warnings = check_deps(&agents, &skills).unwrap();
        // Malformed YAML → empty skills → no missing skill warnings
        assert!(warnings.is_empty());
    }

    #[test]
    fn missing_skill_with_suggestion() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(
            dir.path(),
            "coder",
            "---\nskills: [plan]\n---\n# Coder\n",
        );

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["planning"].iter().map(|s| s.to_string()).collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert_eq!(warnings.len(), 2); // 1 MissingSkill + 1 OrphanedSkill

        // Find the MissingSkill warning
        let missing = warnings
            .iter()
            .find(|w| matches!(w, ValidationWarning::MissingSkill { .. }))
            .unwrap();

        match missing {
            ValidationWarning::MissingSkill { suggestion, .. } => {
                assert_eq!(suggestion.as_deref(), Some("planning"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn suggestion_reverse_substring() {
        // "planning" contains "plan" → suggestion
        let available: HashSet<String> = ["planning"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_suggestion("plan", &available), Some("planning".to_string()));
    }

    #[test]
    fn suggestion_forward_substring() {
        // "review-pr" contains "review" → suggestion
        let available: HashSet<String> = ["review"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            find_suggestion("review-pr", &available),
            Some("review".to_string())
        );
    }

    #[test]
    fn suggestion_case_insensitive() {
        let available: HashSet<String> = ["Planning"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            find_suggestion("plan", &available),
            Some("Planning".to_string())
        );
    }

    #[test]
    fn no_suggestion_when_no_match() {
        let available: HashSet<String> = ["review"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_suggestion("completely-different", &available), None);
    }

    #[test]
    fn multiple_agents_multiple_warnings() {
        let dir = TempDir::new().unwrap();
        let p1 = write_agent(
            dir.path(),
            "coder",
            "---\nskills: [missing-a, existing]\n---\n# Coder\n",
        );
        let p2 = write_agent(
            dir.path(),
            "reviewer",
            "---\nskills: [missing-b]\n---\n# Reviewer\n",
        );

        let agents = vec![
            ("coder".to_string(), p1),
            ("reviewer".to_string(), p2),
        ];
        let skills: HashSet<String> = ["existing", "orphan"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let warnings = check_deps(&agents, &skills).unwrap();

        // Count by type
        let missing_count = warnings
            .iter()
            .filter(|w| matches!(w, ValidationWarning::MissingSkill { .. }))
            .count();
        let orphan_count = warnings
            .iter()
            .filter(|w| matches!(w, ValidationWarning::OrphanedSkill { .. }))
            .count();

        assert_eq!(missing_count, 2); // missing-a, missing-b
        assert_eq!(orphan_count, 1); // orphan (existing is referenced)
    }

    #[test]
    fn empty_agents_and_skills() {
        let agents: Vec<(String, PathBuf)> = vec![];
        let skills: HashSet<String> = HashSet::new();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn unreadable_agent_file_treated_as_no_skills() {
        // Path to a file that doesn't exist — check_deps should not crash
        let agents = vec![(
            "ghost".to_string(),
            PathBuf::from("/nonexistent/ghost.md"),
        )];
        let skills: HashSet<String> = HashSet::new();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn skills_with_dunder_prefix() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(
            dir.path(),
            "coder",
            "---\nskills:\n  - __meridian-spawn\n  - planning\n---\n# Coder\n",
        );

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["__meridian-spawn", "planning"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
    }
}
