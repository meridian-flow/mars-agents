//! Frontmatter skill reference rewriting after collision-driven renames.
//!
//! When a collision forces a rename and affected agents have frontmatter
//! `skills:` references to the renamed skill, this module rewrites those
//! references to point at the correct renamed version.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::error::MarsError;
use crate::frontmatter;
use crate::lock::ItemKind;
use crate::resolve::ResolvedGraph;
use crate::sync::target::{ExplicitSkillRename, TargetState};
use crate::types::{DestPath, ItemName, SourceName};

/// Rewrite frontmatter skill references for renamed transitive deps.
///
/// When a collision forces a rename AND affected agents have frontmatter
/// `skills:` references to the renamed skill, mars rewrites those references
/// to point at the correct renamed version.
pub fn rewrite_skill_refs(
    target: &mut TargetState,
    renames: &[ExplicitSkillRename],
    graph: &ResolvedGraph,
) -> Result<Vec<String>, MarsError> {
    let mut warnings = Vec::new();

    if renames.is_empty() {
        return Ok(warnings);
    }

    // Build rename map for skills only:
    // original skill name -> [(renamed skill name, source name)].
    let mut skill_renames: HashMap<ItemName, Vec<(ItemName, SourceName)>> = HashMap::new();
    for ra in renames {
        let is_skill = target
            .items
            .values()
            .any(|item| item.id.kind == ItemKind::Skill && item.id.name == ra.new_name);
        if is_skill {
            skill_renames
                .entry(ra.original_name.clone())
                .or_default()
                .push((ra.new_name.clone(), ra.source_name.clone()));
        }
    }

    if skill_renames.is_empty() {
        return Ok(warnings);
    }

    // For each agent in target, check if it references any renamed skills
    let agent_keys: Vec<DestPath> = target
        .items
        .iter()
        .filter(|(_, item)| item.id.kind == ItemKind::Agent)
        .map(|(key, _)| key.clone())
        .collect();

    for key in agent_keys {
        let (source_path, source_name) = {
            let item = &target.items[&key];
            (item.source_path.clone(), item.source_name.clone())
        };
        let content = match std::fs::read_to_string(&source_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut renames_for_agent: IndexMap<String, String> = IndexMap::new();
        let agent_deps: &[SourceName] = graph
            .nodes
            .get(&source_name)
            .map(|n| n.deps.as_slice())
            .unwrap_or(&[]);

        for (original_name, entries) in &skill_renames {
            let selected = entries
                .iter()
                .find(|(_, source)| source == &source_name)
                .or_else(|| {
                    entries
                        .iter()
                        .find(|(_, source)| agent_deps.contains(source))
                });
            if let Some((new_name, _)) = selected {
                renames_for_agent.insert(original_name.to_string(), new_name.to_string());
            }
        }
        if renames_for_agent.is_empty() {
            continue;
        }

        match frontmatter::rewrite_content_skills(&content, &renames_for_agent) {
            Ok(Some(new_content)) => {
                if let Some(target_item) = target.items.get_mut(&key) {
                    target_item.rewritten_content = Some(new_content);
                }
            }
            Ok(None) => {}
            Err(e) => {
                warnings.push(format!(
                    "warning: could not rewrite skill refs in {}: {e}",
                    source_path.display()
                ));
            }
        }
    }

    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash;
    use crate::lock::{ItemId, ItemKind};
    use crate::resolve::ResolvedGraph;
    use crate::sync::target::{ExplicitSkillRename, TargetItem, TargetState};
    use crate::types::SourceId;
    use indexmap::IndexMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn rewrite_skill_refs_uses_exact_skill_matches() {
        let dir = TempDir::new().unwrap();
        let agent_path = dir.path().join("agents/coder.md");
        fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        fs::write(
            &agent_path,
            "---\nskills:\n- plan\n- planner\n---\n# Agent\n",
        )
        .unwrap();

        let skill_path = dir.path().join("skills/plan__org_base");
        fs::create_dir_all(&skill_path).unwrap();
        fs::write(skill_path.join("SKILL.md"), "# Planning").unwrap();

        let mut items = IndexMap::new();
        items.insert(
            "agents/coder.md".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Agent,
                    name: "coder".into(),
                },
                source_name: "source-a".into(),
                origin: crate::types::SourceOrigin::Dependency("source-a".into()),
                source_id: SourceId::Path {
                    canonical: agent_path.clone(),
                    subpath: None,
                },
                source_path: agent_path.clone(),
                dest_path: "agents/coder.md".into(),
                source_hash: hash::hash_bytes(fs::read(&agent_path).unwrap().as_slice()).into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );
        items.insert(
            "skills/plan__org_base".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Skill,
                    name: "plan__org_base".into(),
                },
                source_name: "source-a".into(),
                origin: crate::types::SourceOrigin::Dependency("source-a".into()),
                source_id: SourceId::Path {
                    canonical: skill_path.clone(),
                    subpath: None,
                },
                source_path: skill_path.clone(),
                dest_path: "skills/plan__org_base".into(),
                source_hash: hash::compute_hash(&skill_path, ItemKind::Skill)
                    .unwrap()
                    .into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );

        let mut target = TargetState { items };
        let renames = vec![ExplicitSkillRename {
            original_name: "plan".into(),
            new_name: "plan__org_base".into(),
            source_name: "source-a".into(),
        }];
        let graph = ResolvedGraph {
            nodes: IndexMap::new(),
            order: vec![],
            id_index: std::collections::HashMap::new(),
            filters: std::collections::HashMap::new(),
        };

        rewrite_skill_refs(&mut target, &renames, &graph).unwrap();

        let rewritten = target.items["agents/coder.md"]
            .rewritten_content
            .as_ref()
            .unwrap();
        let fm = crate::frontmatter::parse(rewritten).unwrap();
        assert_eq!(fm.skills(), vec!["plan__org_base", "planner"]);
    }

    #[test]
    fn rewrite_skill_refs_leaves_non_matching_agents_unchanged() {
        let dir = TempDir::new().unwrap();
        let agent_path = dir.path().join("agents/coder.md");
        fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        fs::write(&agent_path, "---\nskills: [review]\n---\n# Agent\n").unwrap();

        let mut items = IndexMap::new();
        items.insert(
            "agents/coder.md".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Agent,
                    name: "coder".into(),
                },
                source_name: "source-a".into(),
                origin: crate::types::SourceOrigin::Dependency("source-a".into()),
                source_id: SourceId::Path {
                    canonical: agent_path.clone(),
                    subpath: None,
                },
                source_path: agent_path.clone(),
                dest_path: "agents/coder.md".into(),
                source_hash: hash::hash_bytes(fs::read(&agent_path).unwrap().as_slice()).into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );

        let mut target = TargetState { items };
        let renames = vec![ExplicitSkillRename {
            original_name: "plan".into(),
            new_name: "plan__org_base".into(),
            source_name: "source-a".into(),
        }];
        let graph = ResolvedGraph {
            nodes: IndexMap::new(),
            order: vec![],
            id_index: std::collections::HashMap::new(),
            filters: std::collections::HashMap::new(),
        };

        rewrite_skill_refs(&mut target, &renames, &graph).unwrap();
        assert!(target.items["agents/coder.md"].rewritten_content.is_none());
    }

    #[test]
    fn rewrite_skill_refs_cross_package_uses_dep_graph() {
        let dir = TempDir::new().unwrap();
        let agent_path = dir.path().join("agents/coder.md");
        fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        fs::write(&agent_path, "---\nskills:\n- planning\n---\n# Agent\n").unwrap();

        let skill_b_path = dir.path().join("skills/planning__org_b");
        fs::create_dir_all(&skill_b_path).unwrap();
        fs::write(skill_b_path.join("SKILL.md"), "# Planning from B").unwrap();

        let skill_c_path = dir.path().join("skills/planning__org_c");
        fs::create_dir_all(&skill_c_path).unwrap();
        fs::write(skill_c_path.join("SKILL.md"), "# Planning from C").unwrap();

        let mut items = IndexMap::new();
        items.insert(
            "agents/coder.md".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Agent,
                    name: "coder".into(),
                },
                source_name: "source-a".into(),
                origin: crate::types::SourceOrigin::Dependency("source-a".into()),
                source_id: SourceId::Path {
                    canonical: agent_path.clone(),
                    subpath: None,
                },
                source_path: agent_path.clone(),
                dest_path: "agents/coder.md".into(),
                source_hash: hash::hash_bytes(fs::read(&agent_path).unwrap().as_slice()).into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );
        items.insert(
            "skills/planning__org_b".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Skill,
                    name: "planning__org_b".into(),
                },
                source_name: "source-b".into(),
                origin: crate::types::SourceOrigin::Dependency("source-b".into()),
                source_id: SourceId::Path {
                    canonical: skill_b_path.clone(),
                    subpath: None,
                },
                source_path: skill_b_path.clone(),
                dest_path: "skills/planning__org_b".into(),
                source_hash: hash::compute_hash(&skill_b_path, ItemKind::Skill)
                    .unwrap()
                    .into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );
        items.insert(
            "skills/planning__org_c".into(),
            TargetItem {
                id: ItemId {
                    kind: ItemKind::Skill,
                    name: "planning__org_c".into(),
                },
                source_name: "source-c".into(),
                origin: crate::types::SourceOrigin::Dependency("source-c".into()),
                source_id: SourceId::Path {
                    canonical: skill_c_path.clone(),
                    subpath: None,
                },
                source_path: skill_c_path.clone(),
                dest_path: "skills/planning__org_c".into(),
                source_hash: hash::compute_hash(&skill_c_path, ItemKind::Skill)
                    .unwrap()
                    .into(),
                is_flat_skill: false,
                rewritten_content: None,
            },
        );

        let mut target = TargetState { items };
        let renames = vec![
            ExplicitSkillRename {
                original_name: "planning".into(),
                new_name: "planning__org_b".into(),
                source_name: "source-b".into(),
            },
            ExplicitSkillRename {
                original_name: "planning".into(),
                new_name: "planning__org_c".into(),
                source_name: "source-c".into(),
            },
        ];

        let mut nodes = IndexMap::new();
        nodes.insert(
            SourceName::from("source-a"),
            crate::resolve::ResolvedNode {
                source_name: "source-a".into(),
                source_id: SourceId::Path {
                    canonical: dir.path().to_path_buf(),
                    subpath: None,
                },
                rooted_ref: crate::resolve::RootedSourceRef {
                    checkout_root: dir.path().to_path_buf(),
                    package_root: dir.path().to_path_buf(),
                },
                resolved_ref: crate::source::ResolvedRef {
                    source_name: "source-a".into(),
                    version: None,
                    version_tag: None,
                    commit: None,
                    tree_path: dir.path().to_path_buf(),
                },
                latest_version: None,
                manifest: None,
                deps: vec!["source-b".into()],
            },
        );
        let graph = ResolvedGraph {
            nodes,
            order: vec!["source-a".into()],
            id_index: std::collections::HashMap::new(),
            filters: std::collections::HashMap::new(),
        };

        rewrite_skill_refs(&mut target, &renames, &graph).unwrap();

        let rewritten = target.items["agents/coder.md"]
            .rewritten_content
            .as_ref()
            .expect("agent should have been rewritten");
        let fm = crate::frontmatter::parse(rewritten).unwrap();
        assert_eq!(fm.skills(), vec!["planning__org_b"]);
    }
}
