use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::frontmatter;
use crate::lock::{ItemId, ItemKind};
use crate::types::ItemName;

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
    match frontmatter::parse(content) {
        Ok(fm) => fm.skills(),
        Err(_) => Vec::new(),
    }
}

/// Check that agent→skill references resolve.
///
/// Reads YAML frontmatter from each agent .md file to extract `skills: [...]`.
/// Checks each referenced skill name exists in `available_skills`.
///
/// Returns warnings, not errors — a missing skill doesn't prevent sync.
pub fn check_deps(
    agents: &[(String, PathBuf)],
    available_skills: &HashSet<String>,
) -> Result<Vec<ValidationWarning>, MarsError> {
    let mut warnings = Vec::new();

    for (agent_name, agent_path) in agents {
        // Defensive: if we can't read/parse the file, treat as no skills
        let skills = parse_agent_skills(agent_path).unwrap_or_default();

        for skill_name in skills {
            if !available_skills.contains(&skill_name) {
                let suggestion = find_suggestion(&skill_name, available_skills);
                warnings.push(ValidationWarning::MissingSkill {
                    agent: ItemId {
                        kind: ItemKind::Agent,
                        name: ItemName::from(agent_name.clone()),
                    },
                    skill_name,
                    suggestion,
                });
            }
        }
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
        let skills: HashSet<String> = ["planning", "review"]
            .iter()
            .map(|s| s.to_string())
            .collect();

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
    fn unreferenced_skill_produces_no_warning() {
        let dir = TempDir::new().unwrap();
        let p = write_agent(dir.path(), "coder", "---\nskills: []\n---\n# Coder\n");

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["unused-skill"].iter().map(|s| s.to_string()).collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert!(warnings.is_empty());
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
        let p = write_agent(dir.path(), "coder", "---\nskills: [plan]\n---\n# Coder\n");

        let agents = vec![("coder".to_string(), p)];
        let skills: HashSet<String> = ["planning"].iter().map(|s| s.to_string()).collect();

        let warnings = check_deps(&agents, &skills).unwrap();
        assert_eq!(warnings.len(), 1); // 1 MissingSkill only

        match &warnings[0] {
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
        assert_eq!(
            find_suggestion("plan", &available),
            Some("planning".to_string())
        );
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

        let agents = vec![("coder".to_string(), p1), ("reviewer".to_string(), p2)];
        let skills: HashSet<String> = ["existing", "orphan"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let warnings = check_deps(&agents, &skills).unwrap();

        // Only MissingSkill warnings — no orphan warnings
        assert_eq!(warnings.len(), 2); // missing-a, missing-b
        assert!(warnings
            .iter()
            .all(|w| matches!(w, ValidationWarning::MissingSkill { .. })));
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
        let agents = vec![("ghost".to_string(), PathBuf::from("/nonexistent/ghost.md"))];
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
