use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::config::{EffectiveConfig, FilterMode};
use crate::discover;
use crate::error::MarsError;
use crate::hash;
use crate::lock::{ItemId, ItemKind, LockFile};
use crate::resolve::ResolvedGraph;
use crate::sync::filter::apply_filter;
use crate::types::{
    ContentHash, DestPath, ItemName, RenameMap, SourceId, SourceName, SourceOrigin,
};

/// What `.agents/` should look like after sync.
///
/// Built from the resolved graph with intent-based filtering applied.
#[derive(Debug, Clone)]
pub struct TargetState {
    /// Keyed by dest_path (relative to .agents/).
    pub items: IndexMap<DestPath, TargetItem>,
}

/// A single item in the desired target state.
#[derive(Debug, Clone)]
pub struct TargetItem {
    pub id: ItemId,
    pub source_name: SourceName,
    pub origin: SourceOrigin,
    pub source_id: SourceId,
    /// Path to content in fetched source tree.
    pub source_path: PathBuf,
    /// Relative path under `.agents/` (reflects rename if any).
    pub dest_path: DestPath,
    /// SHA-256 of source content.
    pub source_hash: ContentHash,
    /// True when this item comes from root-level `SKILL.md` flat skill discovery.
    pub is_flat_skill: bool,
    /// Optional in-memory content override after frontmatter rewrites.
    pub rewritten_content: Option<String>,
}

/// Rename action produced by collision detection.
#[derive(Debug, Clone)]
pub struct RenameAction {
    pub original_name: ItemName,
    pub new_name: ItemName,
    pub source_name: SourceName,
}

/// Build target state with collision detection integrated.
///
/// This is the main entry point — it builds the target, detects collisions,
/// applies auto-renames, and returns both the target state and rename actions.
pub fn build_with_collisions(
    graph: &ResolvedGraph,
    config: &EffectiveConfig,
) -> Result<(TargetState, Vec<RenameAction>), MarsError> {
    // Phase 1: Collect all items without dedup
    let mut all_items: Vec<TargetItem> = Vec::new();

    for source_name in &graph.order {
        let node = &graph.nodes[source_name];
        let source_config = config.dependencies.get(source_name);

        let discovered =
            discover::discover_source(&node.resolved_ref.tree_path, Some(source_name.as_str()))?;

        let source_id = source_config
            .map(|s| s.id.clone())
            .unwrap_or_else(|| node.source_id.clone());

        let filters = graph
            .filters
            .get(source_name)
            .filter(|filters| !filters.is_empty())
            .cloned()
            .or_else(|| source_config.map(|source| vec![source.filter.clone()]))
            .unwrap_or_else(|| vec![FilterMode::All]);

        let renames = source_config
            .map(|s| &s.rename)
            .cloned()
            .unwrap_or_default();

        let filtered = apply_filter_union(&discovered, &filters, &node.resolved_ref.tree_path)?;

        for item in filtered {
            let is_flat_skill =
                item.id.kind == ItemKind::Skill && item.source_path == Path::new(".");
            let source_content_path = node.resolved_ref.tree_path.join(&item.source_path);
            let source_hash = if is_flat_skill {
                ContentHash::from(hash::compute_skill_hash_filtered(
                    &source_content_path,
                    crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL,
                )?)
            } else {
                ContentHash::from(hash::compute_hash(&source_content_path, item.id.kind)?)
            };

            let (dest_name, dest_path) = apply_item_rename(item.id.kind, &item.id.name, &renames);

            all_items.push(TargetItem {
                id: ItemId {
                    kind: item.id.kind,
                    name: dest_name,
                },
                source_name: source_name.clone(),
                origin: SourceOrigin::Dependency(source_name.clone()),
                source_id: source_id.clone(),
                source_path: source_content_path,
                dest_path,
                source_hash,
                is_flat_skill,
                rewritten_content: None,
            });
        }
    }

    // Phase 2: Detect collisions on dest_path
    let mut dest_counts: HashMap<DestPath, Vec<usize>> = HashMap::new();
    for (idx, item) in all_items.iter().enumerate() {
        let key = item.dest_path.clone();
        dest_counts.entry(key).or_default().push(idx);
    }

    let mut rename_actions = Vec::new();

    // Phase 3: Apply auto-rename for collisions
    for indices in dest_counts.values() {
        if indices.len() <= 1 {
            continue;
        }

        // Collision detected — rename all colliding items
        for &idx in indices {
            let item = &all_items[idx];
            let original_name = item.id.name.clone();

            // Extract owner_repo suffix from source URL or source name
            let suffix = extract_owner_repo_from_id(&item.source_id, &item.source_name);
            let new_name = format!("{original_name}__{suffix}");
            let new_item_name = ItemName::from(new_name.clone());

            let new_dest_path = DestPath::from(match item.id.kind {
                ItemKind::Agent => PathBuf::from("agents").join(format!("{new_name}.md")),
                ItemKind::Skill => PathBuf::from("skills").join(&new_name),
            });

            rename_actions.push(RenameAction {
                original_name: original_name.clone(),
                new_name: new_item_name.clone(),
                source_name: item.source_name.clone(),
            });

            // Apply rename in-place
            let item_mut = &mut all_items[idx];
            item_mut.id.name = new_item_name;
            item_mut.dest_path = new_dest_path;
        }
    }

    // Phase 4: Build final TargetState from (possibly renamed) items
    let mut items = IndexMap::new();
    for item in all_items {
        let key = item.dest_path.clone();
        items.insert(key, item);
    }

    Ok((TargetState { items }, rename_actions))
}

fn apply_filter_union(
    discovered: &[discover::DiscoveredItem],
    filters: &[FilterMode],
    tree_path: &Path,
) -> Result<Vec<discover::DiscoveredItem>, MarsError> {
    if filters.is_empty() {
        return Ok(discovered.to_vec());
    }

    let mut union: HashSet<(ItemKind, ItemName, PathBuf)> = HashSet::new();
    for filter in filters {
        let filtered = apply_filter(discovered, filter, tree_path)?;
        union.extend(
            filtered
                .iter()
                .map(|item| (item.id.kind, item.id.name.clone(), item.source_path.clone())),
        );
    }

    Ok(discovered
        .iter()
        .filter(|item| {
            union.contains(&(item.id.kind, item.id.name.clone(), item.source_path.clone()))
        })
        .cloned()
        .collect())
}

// Re-export for API compatibility — rewrite_skill_refs moved to sync::rewrite.
pub use crate::sync::rewrite::rewrite_skill_refs;

/// Existing on-disk destination that is not lock-managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmanagedCollision {
    pub source_name: SourceName,
    pub path: DestPath,
}

/// Detect target installs that would overwrite unmanaged on-disk content.
///
/// If a target destination already exists but is not tracked in the lock file,
/// treat it as user-authored content and report it as a collision so callers can
/// skip installation while leaving existing files untouched.
pub fn check_unmanaged_collisions(
    install_target: &Path,
    lock: &LockFile,
    target: &TargetState,
) -> Vec<UnmanagedCollision> {
    let mut collisions = Vec::new();

    for (dest_key, target_item) in &target.items {
        if lock.items.contains_key(dest_key) {
            continue;
        }

        let disk_path = install_target.join(&target_item.dest_path);
        if disk_path.exists() {
            // Check if disk content matches what we'd install — if so,
            // this is a partial prior install (crash recovery), not an
            // unmanaged user file. Safe to overwrite.
            if let Ok(disk_hash) = hash::compute_hash(&disk_path, target_item.id.kind)
                && disk_hash == target_item.source_hash.as_str()
            {
                continue;
            }

            collisions.push(UnmanagedCollision {
                source_name: target_item.source_name.clone(),
                path: target_item.dest_path.clone(),
            });
        }
    }

    collisions
}

fn apply_item_rename(kind: ItemKind, item_name: &str, renames: &RenameMap) -> (ItemName, DestPath) {
    let default_dest = default_dest_path(kind, item_name);
    let default_key = default_dest.to_string_lossy().to_string();

    let rename_value = renames.get(&default_key).or_else(|| renames.get(item_name));

    let dest_path = match rename_value {
        Some(value) => parse_rename_dest(kind, value.as_str()),
        None => default_dest,
    };
    let dest_name = dest_name_from_path(kind, &dest_path);

    (ItemName::from(dest_name), DestPath::from(dest_path))
}

fn default_dest_path(kind: ItemKind, name: &str) -> PathBuf {
    match kind {
        ItemKind::Agent => PathBuf::from("agents").join(format!("{name}.md")),
        ItemKind::Skill => PathBuf::from("skills").join(name),
    }
}

fn parse_rename_dest(kind: ItemKind, rename_value: &str) -> PathBuf {
    let value = PathBuf::from(rename_value);
    let has_prefix = value.starts_with("agents") || value.starts_with("skills");
    let has_parent = value.parent().is_some_and(|p| p != Path::new(""));

    if has_prefix || has_parent {
        return value;
    }

    match kind {
        ItemKind::Agent => {
            if rename_value.ends_with(".md") {
                PathBuf::from("agents").join(rename_value)
            } else {
                PathBuf::from("agents").join(format!("{rename_value}.md"))
            }
        }
        ItemKind::Skill => PathBuf::from("skills").join(rename_value),
    }
}

fn dest_name_from_path(kind: ItemKind, path: &Path) -> String {
    match kind {
        ItemKind::Agent => path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        ItemKind::Skill => path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

/// Extract `{owner}_{repo}` from a source URL.
///
/// For git URLs like `github.com/meridian-flow/meridian-base`, extracts `meridian-flow_meridian-base`.
/// For path sources, uses the source name directly.
pub fn extract_owner_repo(url: Option<&str>, source_name: &str) -> String {
    if let Some(url) = url {
        // Try to extract from URL patterns:
        // github.com/owner/repo, https://github.com/owner/repo.git, etc.
        let cleaned = url.trim_end_matches('/').trim_end_matches(".git");

        // Strip protocol
        let without_proto = cleaned
            .strip_prefix("https://")
            .or_else(|| cleaned.strip_prefix("http://"))
            .or_else(|| cleaned.strip_prefix("ssh://"))
            .or_else(|| cleaned.strip_prefix("git://"))
            .unwrap_or(cleaned);

        // Handle git@ SSH format: git@github.com:owner/repo
        let normalized = if let Some(rest) = without_proto.strip_prefix("git@") {
            rest.replacen(':', "/", 1)
        } else {
            without_proto.to_string()
        };

        // Split by '/' and take last two parts as owner/repo
        let parts: Vec<&str> = normalized.split('/').collect();
        if parts.len() >= 2 {
            let owner = parts[parts.len() - 2];
            let repo = parts[parts.len() - 1];
            return format!("{owner}_{repo}");
        }
    }

    // Fallback: use source name
    source_name.to_string()
}

fn extract_owner_repo_from_id(source_id: &SourceId, source_name: &str) -> String {
    match source_id {
        SourceId::Git { url } => extract_owner_repo(Some(url.as_ref()), source_name),
        SourceId::Path { .. } => extract_owner_repo(None, source_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::lock::LockFile;
    use crate::resolve::{ResolvedGraph, ResolvedNode};
    use crate::source::ResolvedRef;
    use indexmap::IndexMap;
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

    fn make_graph_and_config(
        sources: Vec<(&str, &TempDir, Option<&str>, FilterMode)>,
    ) -> (ResolvedGraph, EffectiveConfig) {
        let mut nodes = IndexMap::new();
        let mut order = Vec::new();
        let mut config_dependencies = IndexMap::new();

        for (name, tree, url, filter) in sources {
            let url_str = url.map(|u| u.to_string());
            nodes.insert(
                name.into(),
                ResolvedNode {
                    source_name: name.into(),
                    source_id: if let Some(u) = url {
                        SourceId::git(crate::types::SourceUrl::from(u))
                    } else {
                        SourceId::Path {
                            canonical: tree.path().to_path_buf(),
                        }
                    },
                    resolved_ref: ResolvedRef {
                        source_name: name.into(),
                        version: None,
                        version_tag: None,
                        commit: None,
                        tree_path: tree.path().to_path_buf(),
                    },
                    latest_version: None,
                    manifest: None,
                    deps: vec![],
                },
            );
            order.push(name.into());

            let spec = if let Some(u) = url {
                SourceSpec::Git(GitSpec {
                    url: crate::types::SourceUrl::from(u),
                    version: None,
                })
            } else {
                SourceSpec::Path(tree.path().to_path_buf())
            };

            config_dependencies.insert(
                name.into(),
                EffectiveDependency {
                    name: name.into(),
                    id: if let Some(u) = url {
                        SourceId::git(crate::types::SourceUrl::from(u))
                    } else {
                        SourceId::Path {
                            canonical: tree.path().to_path_buf(),
                        }
                    },
                    spec,
                    filter,
                    rename: RenameMap::new(),
                    is_overridden: false,
                    original_git: url_str.map(|u| GitSpec {
                        url: crate::types::SourceUrl::from(u),
                        version: None,
                    }),
                },
            );
        }

        let graph = ResolvedGraph {
            nodes,
            order,
            id_index: std::collections::HashMap::new(),
            filters: std::collections::HashMap::new(),
        };
        let config = EffectiveConfig {
            dependencies: config_dependencies,
            settings: Settings::default(),
        };
        (graph, config)
    }

    // === extract_owner_repo tests ===

    #[test]
    fn extract_github_https_url() {
        let result = extract_owner_repo(
            Some("https://github.com/meridian-flow/meridian-base"),
            "base",
        );
        assert_eq!(result, "meridian-flow_meridian-base");
    }

    #[test]
    fn extract_github_https_with_git_suffix() {
        let result = extract_owner_repo(
            Some("https://github.com/meridian-flow/meridian-base.git"),
            "base",
        );
        assert_eq!(result, "meridian-flow_meridian-base");
    }

    #[test]
    fn extract_github_ssh_url() {
        let result = extract_owner_repo(
            Some("git@github.com:meridian-flow/meridian-base.git"),
            "base",
        );
        assert_eq!(result, "meridian-flow_meridian-base");
    }

    #[test]
    fn extract_bare_github_url() {
        let result = extract_owner_repo(Some("github.com/someone/cool-agents"), "cool");
        assert_eq!(result, "someone_cool-agents");
    }

    #[test]
    fn extract_fallback_to_source_name() {
        let result = extract_owner_repo(None, "my-source");
        assert_eq!(result, "my-source");
    }

    #[test]
    fn extract_from_short_url() {
        let result = extract_owner_repo(Some("single-segment"), "fallback");
        assert_eq!(result, "fallback");
    }

    // === Target build tests ===

    #[test]
    fn build_single_source_no_filter() {
        let tree = make_source_tree(&[("coder.md", "# coder")], &[("planning", "# planning")]);
        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            Some("https://github.com/org/base"),
            FilterMode::All,
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 2);
        assert!(target.items.contains_key("agents/coder.md"));
        assert!(target.items.contains_key("skills/planning"));
    }

    #[test]
    fn build_with_path_rename_mapping() {
        let tree = make_source_tree(&[("old-name.md", "# old")], &[]);

        let (graph, mut config) = make_graph_and_config(vec![(
            "base",
            &tree,
            Some("https://github.com/org/base"),
            FilterMode::All,
        )]);

        // Add rename mapping
        config
            .dependencies
            .get_mut("base")
            .unwrap()
            .rename
            .insert("agents/old-name.md".into(), "agents/new-name.md".into());

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 1);
        assert!(target.items.contains_key("agents/new-name.md"));
        assert_eq!(target.items["agents/new-name.md"].id.name, "new-name");
    }

    // === Collision tests ===

    #[test]
    fn collision_auto_renames_both() {
        let tree1 = make_source_tree(&[("coder.md", "# coder from source 1")], &[]);
        let tree2 = make_source_tree(&[("coder.md", "# coder from source 2")], &[]);

        let (graph, config) = make_graph_and_config(vec![
            (
                "source-a",
                &tree1,
                Some("https://github.com/alice/agents"),
                FilterMode::All,
            ),
            (
                "source-b",
                &tree2,
                Some("https://github.com/bob/agents"),
                FilterMode::All,
            ),
        ]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert_eq!(renames.len(), 2);
        assert_eq!(target.items.len(), 2);

        // Both should have been renamed
        let names: Vec<&str> = target.items.values().map(|i| i.id.name.as_str()).collect();
        assert!(names.contains(&"coder__alice_agents"));
        assert!(names.contains(&"coder__bob_agents"));
    }

    #[test]
    fn no_collision_no_renames() {
        let tree1 = make_source_tree(&[("coder.md", "# coder")], &[]);
        let tree2 = make_source_tree(&[("reviewer.md", "# reviewer")], &[]);

        let (graph, config) = make_graph_and_config(vec![
            (
                "source-a",
                &tree1,
                Some("https://github.com/alice/agents"),
                FilterMode::All,
            ),
            (
                "source-b",
                &tree2,
                Some("https://github.com/bob/agents"),
                FilterMode::All,
            ),
        ]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 2);
    }

    // === Source with agents filter + skill deps ===

    #[test]
    fn build_with_agents_filter_pulls_transitive_skills() {
        let tree = make_source_tree(
            &[("coder.md", "---\nskills:\n  - planning\n---\n# Coder\n")],
            &[("planning", "# Planning"), ("unused-skill", "# Unused")],
        );

        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            None,
            FilterMode::Include {
                agents: vec!["coder".into()],
                skills: vec![],
            },
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 2); // coder + planning
        assert!(target.items.contains_key("agents/coder.md"));
        assert!(target.items.contains_key("skills/planning"));
        // unused-skill should NOT be present
        assert!(!target.items.contains_key("skills/unused-skill"));
    }

    #[test]
    fn build_with_exclude_filter() {
        let tree = make_source_tree(&[("coder.md", "# coder"), ("deprecated.md", "# old")], &[]);

        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            None,
            FilterMode::Exclude(vec!["deprecated".into()]),
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 1);
        assert!(target.items.contains_key("agents/coder.md"));
    }

    #[test]
    fn build_unions_multiple_include_filters_for_same_source() {
        let tree = make_source_tree(
            &[],
            &[
                ("skill-a", "# Skill A"),
                ("skill-b", "# Skill B"),
                ("skill-c", "# Skill C"),
            ],
        );

        let (mut graph, config) =
            make_graph_and_config(vec![("base", &tree, None, FilterMode::All)]);
        graph.filters.insert(
            "base".into(),
            vec![
                FilterMode::Include {
                    agents: vec![],
                    skills: vec!["skill-a".into(), "skill-b".into()],
                },
                FilterMode::Include {
                    agents: vec![],
                    skills: vec!["skill-b".into(), "skill-c".into()],
                },
            ],
        );

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 3);
        assert!(target.items.contains_key("skills/skill-a"));
        assert!(target.items.contains_key("skills/skill-b"));
        assert!(target.items.contains_key("skills/skill-c"));
    }

    #[test]
    fn build_target_items_have_correct_hashes() {
        let content = "# agent content for hash test";
        let tree = make_source_tree(&[("test.md", content)], &[]);

        let (graph, config) = make_graph_and_config(vec![("base", &tree, None, FilterMode::All)]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        let item = &target.items["agents/test.md"];
        let expected_hash = hash::hash_bytes(content.as_bytes());
        assert_eq!(item.source_hash, expected_hash);
    }

    #[test]
    fn unmanaged_disk_path_collision_reported() {
        let tree = make_source_tree(&[("coder.md", "# managed")], &[]);
        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            Some("https://github.com/org/base"),
            FilterMode::All,
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        let install_root = TempDir::new().unwrap();

        // Existing user-authored file at the same destination.
        let existing = install_root.path().join("agents").join("coder.md");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, "# user-authored").unwrap();

        let collisions =
            check_unmanaged_collisions(install_root.path(), &LockFile::empty(), &target);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].source_name.as_ref(), "base");
        assert_eq!(
            collisions[0].path.as_ref().to_string_lossy(),
            "agents/coder.md"
        );
    }

    #[test]
    fn unmanaged_collision_skipped_when_hash_matches() {
        let content = "# managed agent";
        let tree = make_source_tree(&[("coder.md", content)], &[]);
        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            Some("https://github.com/org/base"),
            FilterMode::All,
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        let install_root = TempDir::new().unwrap();

        // Simulate partial prior install: file on disk with same content
        let existing = install_root.path().join("agents").join("coder.md");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, content).unwrap();

        // Should skip collision — disk content matches planned install (crash recovery)
        let collisions =
            check_unmanaged_collisions(install_root.path(), &LockFile::empty(), &target);
        assert!(collisions.is_empty());
    }

    #[test]
    fn unmanaged_collision_reported_on_different_content() {
        let tree = make_source_tree(&[("coder.md", "# managed")], &[]);
        let (graph, config) = make_graph_and_config(vec![(
            "base",
            &tree,
            Some("https://github.com/org/base"),
            FilterMode::All,
        )]);

        let (target, renames) = build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        let install_root = TempDir::new().unwrap();

        // User-authored file with different content
        let existing = install_root.path().join("agents").join("coder.md");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, "# different user content").unwrap();

        let collisions =
            check_unmanaged_collisions(install_root.path(), &LockFile::empty(), &target);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].source_name.as_ref(), "base");
        assert_eq!(
            collisions[0].path.as_ref().to_string_lossy(),
            "agents/coder.md"
        );
    }
}
