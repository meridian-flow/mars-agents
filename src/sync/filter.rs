//! Intent-based filtering for discovered items.
//!
//! Applies filter modes (All, Exclude, Include, OnlySkills, OnlyAgents) to
//! discovered items, including transitive skill dependency resolution via
//! agent frontmatter parsing.

use std::collections::HashSet;
use std::path::Path;

use crate::config::FilterMode;
use crate::discover;
use crate::error::MarsError;
use crate::lock::ItemKind;
use crate::types::ItemName;
use crate::validate;

/// Apply filter mode to discovered items.
///
/// For Include mode with agents: also resolves transitive skill dependencies
/// by parsing agent frontmatter.
pub(crate) fn apply_filter(
    discovered: &[discover::DiscoveredItem],
    filter: &FilterMode,
    package_root: &Path,
) -> Result<Vec<discover::DiscoveredItem>, MarsError> {
    match filter {
        FilterMode::All => Ok(discovered.to_vec()),

        FilterMode::Exclude(excluded) => Ok(discovered
            .iter()
            .filter(|item| {
                let path_str = item.source_path.to_string_lossy();
                !excluded.iter().any(|e| {
                    // Match against full source path or just the name
                    crate::target::paths_equivalent(&path_str, e.as_ref()) || item.id.name == *e
                })
            })
            .cloned()
            .collect()),

        FilterMode::Include { agents, skills } => {
            // Start with explicitly requested items
            let mut include_set: HashSet<ItemName> = HashSet::new();

            // Add explicitly requested agents and skills
            for a in agents {
                include_set.insert(a.clone());
            }
            for s in skills {
                include_set.insert(s.clone());
            }

            // Resolve transitive skill deps from agent frontmatter
            resolve_agent_skill_deps(discovered, agents, package_root, &mut include_set);

            Ok(discovered
                .iter()
                .filter(|item| include_set.contains(&item.id.name))
                .cloned()
                .collect())
        }

        FilterMode::OnlySkills => Ok(discovered
            .iter()
            .filter(|item| item.id.kind == ItemKind::Skill)
            .cloned()
            .collect()),

        FilterMode::OnlyAgents => {
            // Collect all agents
            let agents: Vec<_> = discovered
                .iter()
                .filter(|item| item.id.kind == ItemKind::Agent)
                .cloned()
                .collect();

            // Resolve transitive skill deps from all agent frontmatter
            let agent_names: Vec<ItemName> = agents.iter().map(|a| a.id.name.clone()).collect();
            let mut skill_deps: HashSet<ItemName> = HashSet::new();
            resolve_agent_skill_deps(discovered, &agent_names, package_root, &mut skill_deps);

            // Include agents + their transitive skill deps only
            let skills: Vec<_> = discovered
                .iter()
                .filter(|item| {
                    item.id.kind == ItemKind::Skill && skill_deps.contains(&item.id.name)
                })
                .cloned()
                .collect();

            let mut result = agents;
            result.extend(skills);
            Ok(result)
        }
    }
}

/// Resolve transitive skill dependencies from agent frontmatter.
///
/// For each agent name, finds the matching discovered item and parses its
/// frontmatter to extract skill dependencies, inserting them into the provided set.
fn resolve_agent_skill_deps(
    discovered: &[discover::DiscoveredItem],
    agent_names: &[ItemName],
    package_root: &Path,
    skill_deps: &mut HashSet<ItemName>,
) {
    for agent_name in agent_names {
        if let Some(agent_item) = discovered
            .iter()
            .find(|i| i.id.kind == ItemKind::Agent && i.id.name == *agent_name)
        {
            let agent_path = package_root.join(&agent_item.source_path);
            let deps = validate::parse_item_skill_deps(&agent_path).unwrap_or_default();
            for skill in deps {
                skill_deps.insert(ItemName::from(skill));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a source tree with agents and skills
    fn make_source_tree(agents: &[(&str, &str)], skills: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        if !agents.is_empty() {
            let agents_dir = dir.path().join("agents");
            fs::create_dir_all(&agents_dir).unwrap();
            for (name, content) in agents {
                fs::write(agents_dir.join(name), content).unwrap();
            }
        }
        if !skills.is_empty() {
            let skills_dir = dir.path().join("skills");
            fs::create_dir_all(&skills_dir).unwrap();
            for (name, content) in skills {
                let skill_dir = skills_dir.join(name);
                fs::create_dir_all(&skill_dir).unwrap();
                fs::write(skill_dir.join("SKILL.md"), content).unwrap();
            }
        }
        dir
    }

    #[test]
    fn filter_all_returns_everything() {
        let tree = make_source_tree(
            &[("coder.md", "# coder"), ("reviewer.md", "# reviewer")],
            &[("planning", "# planning")],
        );
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(&discovered, &FilterMode::All, tree.path()).unwrap();
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn filter_exclude_removes_items() {
        let tree = make_source_tree(
            &[("coder.md", "# coder"), ("reviewer.md", "# reviewer")],
            &[],
        );
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(
            &discovered,
            &FilterMode::Exclude(vec!["reviewer".into()]),
            tree.path(),
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id.name, "coder");
    }

    #[cfg(windows)]
    #[test]
    fn filter_exclude_path_matches_mixed_separators_on_windows() {
        let tree = make_source_tree(&[("coder.md", "# coder")], &[]);
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(
            &discovered,
            &FilterMode::Exclude(vec![r"agents\coder.md".into()]),
            tree.path(),
        )
        .unwrap();

        assert!(filtered.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn filter_exclude_path_preserves_backslash_on_posix() {
        let tree = make_source_tree(&[("coder.md", "# coder")], &[]);
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(
            &discovered,
            &FilterMode::Exclude(vec![r"agents\coder.md".into()]),
            tree.path(),
        )
        .unwrap();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id.name, "coder");
    }

    #[test]
    fn filter_include_agents_only() {
        let tree = make_source_tree(
            &[("coder.md", "# coder"), ("reviewer.md", "# reviewer")],
            &[("planning", "# planning")],
        );
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(
            &discovered,
            &FilterMode::Include {
                agents: vec!["coder".into()],
                skills: vec![],
            },
            tree.path(),
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id.name, "coder");
    }

    #[test]
    fn filter_include_with_transitive_skill_deps() {
        let tree = make_source_tree(
            &[(
                "coder.md",
                "---\nskills:\n  - planning\n---\n# Coder agent\n",
            )],
            &[
                ("planning", "# Planning skill"),
                ("review", "# Review skill"),
            ],
        );
        let discovered = discover::discover_source(tree.path(), None).unwrap();
        let filtered = apply_filter(
            &discovered,
            &FilterMode::Include {
                agents: vec!["coder".into()],
                skills: vec![],
            },
            tree.path(),
        )
        .unwrap();
        // Should include coder agent + planning skill (transitive dep)
        assert_eq!(filtered.len(), 2);
        let names: Vec<&str> = filtered.iter().map(|i| i.id.name.as_str()).collect();
        assert!(names.contains(&"coder"));
        assert!(names.contains(&"planning"));
    }
}
