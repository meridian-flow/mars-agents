use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind};
use crate::types::ItemName;

/// An item discovered in a source tree by filesystem convention.
///
/// Discovery scans for `agents/*.md` and `skills/*/SKILL.md`.
/// The manifest is not consulted for what a package provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredItem {
    pub id: ItemId,
    /// Path within source tree (relative), e.g. "agents/coder.md" or "skills/planning".
    pub source_path: PathBuf,
}

/// Discover all installable items in a source tree by filesystem convention.
///
/// Convention:
/// - `agents/*.md` files become `ItemKind::Agent` items
/// - `skills/*/SKILL.md` directories become `ItemKind::Skill` items
/// - If neither is found, a root-level `SKILL.md` is treated as one flat skill
/// - Everything else is ignored
///
/// Sources without a `mars.toml` work identically — discovery doesn't
/// depend on the manifest.
pub fn discover_source(
    tree_path: &Path,
    fallback_name: Option<&str>,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    let mut items = Vec::new();

    // Discover agents: agents/*.md (non-recursive)
    let agents_dir = tree_path.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Skip hidden files
            if name_str.starts_with('.') {
                continue;
            }

            let path = entry.path();
            if path.is_file()
                && let (Some(ext), Some(stem)) = (path.extension(), path.file_stem())
                && ext == "md"
            {
                items.push(DiscoveredItem {
                    id: ItemId {
                        kind: ItemKind::Agent,
                        name: ItemName::from(stem.to_string_lossy().into_owned()),
                    },
                    source_path: PathBuf::from("agents").join(&file_name),
                });
            }
        }
    }

    // Discover skills: skills/*/SKILL.md (non-recursive)
    let skills_dir = tree_path.join("skills");
    if skills_dir.is_dir() {
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            let dir_name = entry.file_name();
            let name_str = dir_name.to_string_lossy();

            // Skip hidden directories
            if name_str.starts_with('.') {
                continue;
            }

            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").is_file() {
                items.push(DiscoveredItem {
                    id: ItemId {
                        kind: ItemKind::Skill,
                        name: ItemName::from(name_str.into_owned()),
                    },
                    source_path: PathBuf::from("skills").join(&dir_name),
                });
            }
        }
    }

    // Flat skill fallback: root SKILL.md means the whole repo is one skill.
    // Only used when no conventional agents/skills were discovered.
    if items.is_empty() && tree_path.join("SKILL.md").is_file() {
        let name = fallback_name.map(String::from).unwrap_or_else(|| {
            tree_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown-skill".to_string())
        });
        items.push(DiscoveredItem {
            id: ItemId {
                kind: ItemKind::Skill,
                name: ItemName::from(name),
            },
            source_path: PathBuf::from("."),
        });
    }

    // Sort by (kind, name) for deterministic ordering.
    // ItemId derives Ord with Agent < Skill, then lexicographic by name.
    items.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(items)
}

/// An installed item with parsed frontmatter metadata.
#[derive(Debug, Clone)]
pub struct InstalledItem {
    pub id: ItemId,
    /// Disk path (absolute) to the installed file/dir.
    pub path: PathBuf,
    /// Parsed frontmatter name (may differ from filename).
    pub frontmatter_name: Option<String>,
    /// Parsed frontmatter description.
    pub description: Option<String>,
    /// Skills referenced in frontmatter (agents only).
    pub skill_refs: Vec<String>,
}

/// Result of scanning an installed managed root.
#[derive(Debug, Clone)]
pub struct InstalledState {
    pub agents: Vec<InstalledItem>,
    pub skills: Vec<InstalledItem>,
}

/// Discover all installed agents and skills in a managed root.
///
/// Scans `agents/*.md` and `skills/*/SKILL.md`, parses frontmatter,
/// and collects metadata.
pub fn discover_installed(root: &Path) -> Result<InstalledState, MarsError> {
    let mut agents = Vec::new();
    let mut skills = Vec::new();

    // Scan agents/*.md
    let agents_dir = root.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Skip hidden files
            if name_str.starts_with('.') {
                continue;
            }

            // Must be a .md file (following symlinks for the check)
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if ext != Some("md") {
                continue;
            }

            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();

            let (frontmatter_name, description, skill_refs) = parse_installed_frontmatter(&path);

            agents.push(InstalledItem {
                id: ItemId {
                    kind: ItemKind::Agent,
                    name: ItemName::from(stem),
                },
                path,
                frontmatter_name,
                description,
                skill_refs,
            });
        }
    }

    // Scan skills/*/SKILL.md
    let skills_dir = root.join("skills");
    if skills_dir.is_dir() {
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            let path = entry.path();
            let dir_name = entry.file_name();
            let name_str = dir_name.to_string_lossy();

            // Skip hidden directories
            if name_str.starts_with('.') {
                continue;
            }

            if !path.is_dir() {
                continue;
            }

            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }

            let (frontmatter_name, description, _) = parse_installed_frontmatter(&skill_md);

            skills.push(InstalledItem {
                id: ItemId {
                    kind: ItemKind::Skill,
                    name: ItemName::from(name_str.into_owned()),
                },
                path,
                frontmatter_name,
                description,
                skill_refs: Vec::new(),
            });
        }
    }

    // Sort for deterministic order
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    skills.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(InstalledState { agents, skills })
}

/// Parse frontmatter from an installed file, returning (name, description, skill_refs).
/// Returns None/empty on parse failure — the item is still discovered.
fn parse_installed_frontmatter(path: &Path) -> (Option<String>, Option<String>, Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None, Vec::new()),
    };
    match crate::frontmatter::parse(&content) {
        Ok(fm) => {
            let name = fm.name().map(str::to_owned);
            let description = fm
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let skill_refs = fm.skills();
            (name, description, skill_refs)
        }
        Err(_) => (None, None, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a source tree with the given agent and skill files.
    fn make_tree(agents: &[&str], skills: &[&str]) -> TempDir {
        let dir = TempDir::new().unwrap();

        if !agents.is_empty() {
            let agents_dir = dir.path().join("agents");
            fs::create_dir_all(&agents_dir).unwrap();
            for name in agents {
                fs::write(agents_dir.join(name), "# agent content").unwrap();
            }
        }

        if !skills.is_empty() {
            let skills_dir = dir.path().join("skills");
            fs::create_dir_all(&skills_dir).unwrap();
            for name in skills {
                let skill_dir = skills_dir.join(name);
                fs::create_dir_all(&skill_dir).unwrap();
                fs::write(skill_dir.join("SKILL.md"), "# skill content").unwrap();
            }
        }

        dir
    }

    #[test]
    fn discover_agents_only() {
        let tree = make_tree(&["coder.md", "reviewer.md"], &[]);
        let items = discover_source(tree.path(), None).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.kind, ItemKind::Agent);
        assert_eq!(items[0].id.name, "coder");
        assert_eq!(items[0].source_path, PathBuf::from("agents/coder.md"));
        assert_eq!(items[1].id.kind, ItemKind::Agent);
        assert_eq!(items[1].id.name, "reviewer");
        assert_eq!(items[1].source_path, PathBuf::from("agents/reviewer.md"));
    }

    #[test]
    fn discover_skills_only() {
        let tree = make_tree(&[], &["planning"]);
        let items = discover_source(tree.path(), None).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.kind, ItemKind::Skill);
        assert_eq!(items[0].id.name, "planning");
        assert_eq!(items[0].source_path, PathBuf::from("skills/planning"));
    }

    #[test]
    fn discover_agents_and_skills() {
        let tree = make_tree(&["coder.md", "reviewer.md"], &["planning", "review"]);
        let items = discover_source(tree.path(), None).unwrap();

        assert_eq!(items.len(), 4);
        // Agents come first (Agent < Skill), sorted by name
        assert_eq!(items[0].id.name, "coder");
        assert_eq!(items[0].id.kind, ItemKind::Agent);
        assert_eq!(items[1].id.name, "reviewer");
        assert_eq!(items[1].id.kind, ItemKind::Agent);
        // Skills next, sorted by name
        assert_eq!(items[2].id.name, "planning");
        assert_eq!(items[2].id.kind, ItemKind::Skill);
        assert_eq!(items[3].id.name, "review");
        assert_eq!(items[3].id.kind, ItemKind::Skill);
    }

    #[test]
    fn empty_tree_no_agents_or_skills_dir() {
        let tree = TempDir::new().unwrap();
        let items = discover_source(tree.path(), None).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn empty_agents_dir() {
        let tree = TempDir::new().unwrap();
        fs::create_dir_all(tree.path().join("agents")).unwrap();
        let items = discover_source(tree.path(), None).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn non_md_files_in_agents_skipped() {
        let tree = TempDir::new().unwrap();
        let agents_dir = tree.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), "# agent").unwrap();
        fs::write(agents_dir.join("notes.txt"), "not an agent").unwrap();
        fs::write(agents_dir.join("config.yaml"), "not an agent").unwrap();
        fs::write(agents_dir.join("README"), "not an agent").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "coder");
    }

    #[test]
    fn skill_dir_without_skill_md_skipped() {
        let tree = TempDir::new().unwrap();
        let skills_dir = tree.path().join("skills");
        let valid = skills_dir.join("planning");
        let invalid = skills_dir.join("incomplete");
        fs::create_dir_all(&valid).unwrap();
        fs::create_dir_all(&invalid).unwrap();
        fs::write(valid.join("SKILL.md"), "# skill").unwrap();
        // incomplete/ has no SKILL.md

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "planning");
    }

    #[test]
    fn hidden_files_skipped() {
        let tree = TempDir::new().unwrap();
        let agents_dir = tree.path().join("agents");
        let skills_dir = tree.path().join("skills");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::create_dir_all(&skills_dir).unwrap();

        // Hidden agent file
        fs::write(agents_dir.join(".hidden.md"), "# hidden").unwrap();
        // Visible agent file
        fs::write(agents_dir.join("visible.md"), "# visible").unwrap();

        // Hidden skill directory
        let hidden_skill = skills_dir.join(".secret");
        fs::create_dir_all(&hidden_skill).unwrap();
        fs::write(hidden_skill.join("SKILL.md"), "# secret").unwrap();

        // Visible skill directory
        let visible_skill = skills_dir.join("planning");
        fs::create_dir_all(&visible_skill).unwrap();
        fs::write(visible_skill.join("SKILL.md"), "# planning").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.name, "visible");
        assert_eq!(items[1].id.name, "planning");
    }

    #[test]
    fn deterministic_ordering() {
        let tree = make_tree(
            &["zebra.md", "alpha.md", "middle.md"],
            &["z-skill", "a-skill"],
        );

        let items1 = discover_source(tree.path(), None).unwrap();
        let items2 = discover_source(tree.path(), None).unwrap();

        // Same order every time
        assert_eq!(items1, items2);

        // Agents first (sorted), then skills (sorted)
        let names: Vec<&str> = items1.iter().map(|i| i.id.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "middle", "zebra", "a-skill", "z-skill"]
        );
    }

    #[test]
    fn subdirectories_in_agents_ignored() {
        let tree = TempDir::new().unwrap();
        let agents_dir = tree.path().join("agents");
        let sub = agents_dir.join("subdir");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.md"), "# nested").unwrap();
        fs::write(agents_dir.join("top.md"), "# top").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "top");
    }

    #[test]
    fn skill_file_not_dir_ignored() {
        // A file named like a skill but not a directory
        let tree = TempDir::new().unwrap();
        let skills_dir = tree.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("not-a-dir"), "# not a skill dir").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn dunder_prefix_skills_discovered() {
        // Skills with __ prefix are common (e.g., __meridian-spawn)
        let tree = make_tree(&[], &["__meridian-spawn", "planning"]);
        let items = discover_source(tree.path(), None).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.name, "__meridian-spawn");
        assert_eq!(items[1].id.name, "planning");
    }

    #[test]
    fn only_agents_dir_exists() {
        let tree = TempDir::new().unwrap();
        let agents_dir = tree.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("coder.md"), "# coder").unwrap();
        // No skills/ dir at all

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "coder");
    }

    #[test]
    fn only_skills_dir_exists() {
        let tree = TempDir::new().unwrap();
        let skills_dir = tree.path().join("skills");
        let planning = skills_dir.join("planning");
        fs::create_dir_all(&planning).unwrap();
        fs::write(planning.join("SKILL.md"), "# planning").unwrap();
        // No agents/ dir at all

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "planning");
    }

    #[test]
    fn flat_skill_repo_discovered() {
        let tree = TempDir::new().unwrap();
        fs::write(tree.path().join("SKILL.md"), "# flat skill").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.kind, ItemKind::Skill);
        assert_eq!(items[0].source_path, PathBuf::from("."));
    }

    #[test]
    fn flat_skill_with_resources() {
        let tree = TempDir::new().unwrap();
        fs::write(tree.path().join("SKILL.md"), "# flat skill").unwrap();
        fs::create_dir_all(tree.path().join("resources")).unwrap();
        fs::write(tree.path().join("resources/guide.md"), "# guide").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.kind, ItemKind::Skill);
        assert_eq!(items[0].source_path, PathBuf::from("."));
    }

    #[test]
    fn flat_skill_uses_fallback_name() {
        let tree = TempDir::new().unwrap();
        fs::write(tree.path().join("SKILL.md"), "# flat skill").unwrap();

        let items = discover_source(tree.path(), Some("my-skill")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "my-skill");
    }

    #[test]
    fn flat_skill_uses_dirname_when_no_fallback() {
        let parent = TempDir::new().unwrap();
        let tree = parent.path().join("demo-skill");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("SKILL.md"), "# flat skill").unwrap();

        let items = discover_source(&tree, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.name, "demo-skill");
    }

    #[test]
    fn nested_structure_ignores_root_skill_md() {
        let tree = TempDir::new().unwrap();
        fs::write(tree.path().join("SKILL.md"), "# root skill").unwrap();
        let planning = tree.path().join("skills/planning");
        fs::create_dir_all(&planning).unwrap();
        fs::write(planning.join("SKILL.md"), "# nested skill").unwrap();

        let items = discover_source(tree.path(), None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.kind, ItemKind::Skill);
        assert_eq!(items[0].id.name, "planning");
        assert_eq!(items[0].source_path, PathBuf::from("skills/planning"));
    }

    // === discover_installed tests ===

    #[test]
    fn discover_installed_finds_agents_and_skills() {
        let root = TempDir::new().unwrap();
        let agents_dir = root.path().join("agents");
        let skills_dir = root.path().join("skills");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::create_dir_all(skills_dir.join("planning")).unwrap();
        fs::write(
            agents_dir.join("coder.md"),
            "---\nname: coder\n---\n# Agent",
        )
        .unwrap();
        fs::write(
            skills_dir.join("planning").join("SKILL.md"),
            "---\nname: planning\n---\n# Skill",
        )
        .unwrap();

        let state = discover_installed(root.path()).unwrap();
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].id.name, "coder");
        assert_eq!(state.skills.len(), 1);
        assert_eq!(state.skills[0].id.name, "planning");
    }

    #[test]
    fn discover_installed_parses_frontmatter() {
        let root = TempDir::new().unwrap();
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(
            agents_dir.join("coder.md"),
            "---\nname: my-coder\ndescription: A coding agent\nskills:\n  - planning\n  - review\n---\n# Agent",
        )
        .unwrap();

        let state = discover_installed(root.path()).unwrap();
        assert_eq!(state.agents.len(), 1);
        let agent = &state.agents[0];
        assert_eq!(agent.frontmatter_name.as_deref(), Some("my-coder"));
        assert_eq!(agent.description.as_deref(), Some("A coding agent"));
        assert_eq!(agent.skill_refs, vec!["planning", "review"]);
    }

    #[test]
    fn discover_installed_handles_missing_frontmatter() {
        let root = TempDir::new().unwrap();
        let agents_dir = root.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("bare.md"), "# No frontmatter").unwrap();

        let state = discover_installed(root.path()).unwrap();
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].id.name, "bare");
        assert!(state.agents[0].frontmatter_name.is_none());
        assert!(state.agents[0].skill_refs.is_empty());
    }
}
