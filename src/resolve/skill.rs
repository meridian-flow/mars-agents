use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::discover;
use crate::error::{MarsError, ResolutionError};
use crate::lock::ItemKind;
use crate::types::{ItemName, SourceName};
use crate::validate;

use super::package::RegisteredPackage;
use super::types::{PendingItem, VersionConstraint};

#[derive(Debug, Clone)]
struct ResolvedSkill {
    package: SourceName,
}

struct SkillResolver<'a> {
    registry: &'a IndexMap<SourceName, RegisteredPackage>,
}

impl<'a> SkillResolver<'a> {
    fn resolve(
        &self,
        skill: &ItemName,
        requester_package: &SourceName,
    ) -> Result<ResolvedSkill, ResolutionError> {
        if self
            .registry
            .get(requester_package)
            .is_some_and(|package| package.has_skill(skill))
        {
            return Ok(ResolvedSkill {
                package: requester_package.clone(),
            });
        }

        let closure_order = self.dependency_closure_order(requester_package);
        let closure_set: HashSet<SourceName> = closure_order.iter().cloned().collect();

        for package_name in &closure_order {
            if self
                .registry
                .get(package_name)
                .is_some_and(|package| package.has_skill(skill))
            {
                return Ok(ResolvedSkill {
                    package: package_name.clone(),
                });
            }
        }

        for (package_name, package) in self.registry {
            if package_name == requester_package || closure_set.contains(package_name) {
                continue;
            }
            if !package.has_skill(skill) {
                continue;
            }
            return Ok(ResolvedSkill {
                package: package_name.clone(),
            });
        }

        let mut searched: Vec<String> = self.registry.keys().map(ToString::to_string).collect();
        searched.sort();
        Err(ResolutionError::SkillNotFound {
            skill: skill.to_string(),
            required_by: requester_package.to_string(),
            searched,
        })
    }

    fn dependency_closure_order(&self, requester_package: &SourceName) -> Vec<SourceName> {
        let Some(requester) = self.registry.get(requester_package) else {
            return Vec::new();
        };

        // Breadth-first traversal keeps direct deps ahead of transitive deps.
        let mut seen: HashSet<SourceName> = HashSet::from([requester_package.clone()]);
        let mut queue: VecDeque<SourceName> = VecDeque::new();
        let mut order = Vec::new();

        for dep in &requester.node.deps {
            if self.registry.contains_key(dep) && seen.insert(dep.clone()) {
                queue.push_back(dep.clone());
            }
        }

        while let Some(package_name) = queue.pop_front() {
            order.push(package_name.clone());

            if let Some(package) = self.registry.get(&package_name) {
                for dep in &package.node.deps {
                    if self.registry.contains_key(dep) && seen.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        order
    }
}

pub(crate) fn parse_pending_item_skill_deps(
    pending_item: &PendingItem,
    package: &RegisteredPackage,
) -> Result<Vec<ItemName>, MarsError> {
    let Some(discovered) = package.item(pending_item.kind, &pending_item.item) else {
        return Ok(Vec::new());
    };
    let item_path =
        discovered_item_markdown_path(&package.node.rooted_ref.package_root, discovered);
    let skill_deps = validate::parse_item_skill_deps(&item_path)?;
    Ok(skill_deps.into_iter().map(ItemName::from).collect())
}

pub(crate) fn discovered_item_markdown_path(
    package_root: &Path,
    item: &discover::DiscoveredItem,
) -> PathBuf {
    match item.id.kind {
        ItemKind::Agent => package_root.join(&item.source_path),
        ItemKind::Skill => {
            if item.source_path == Path::new(".") {
                package_root.join("SKILL.md")
            } else {
                package_root.join(&item.source_path).join("SKILL.md")
            }
        }
    }
}

pub(crate) fn resolve_skill_ref(
    skill: &ItemName,
    requester: &PendingItem,
    registry: &IndexMap<SourceName, RegisteredPackage>,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
) -> Result<PendingItem, MarsError> {
    let required_by = format!("{}/{}", requester.package, requester.item);
    let resolver = SkillResolver { registry };
    let resolved_skill = match resolver.resolve(skill, &requester.package) {
        Ok(resolved_skill) => resolved_skill,
        Err(ResolutionError::SkillNotFound {
            skill: missing_skill,
            searched,
            ..
        }) => {
            return Err(ResolutionError::SkillNotFound {
                skill: missing_skill,
                required_by,
                searched,
            }
            .into());
        }
        Err(err) => return Err(err.into()),
    };

    let package = registry
        .get(&resolved_skill.package)
        .ok_or_else(|| ResolutionError::SourceNotFound {
            name: resolved_skill.package.to_string(),
        })?;
    let constraint = primary_package_constraint(constraints, &resolved_skill.package)
        .unwrap_or(&package.constraint)
        .clone();
    Ok(PendingItem {
        package: resolved_skill.package,
        item: skill.clone(),
        kind: ItemKind::Skill,
        constraint,
        required_by,
        is_local: package.is_local,
        spec: package.spec.clone(),
    })
}

pub(crate) fn primary_package_constraint<'a>(
    constraints: &'a HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    package: &SourceName,
) -> Option<&'a VersionConstraint> {
    constraints
        .get(package)
        .and_then(|entries| entries.first().map(|(_, constraint)| constraint))
}
