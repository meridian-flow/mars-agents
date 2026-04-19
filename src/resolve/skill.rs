use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::discover;
use crate::error::{MarsError, ResolutionError};
use crate::lock::ItemKind;
use crate::types::{ItemName, SourceName};
use crate::validate;

use super::package::RegisteredPackage;
use super::types::{PendingItem, VersionConstraint};

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

    if let Some(requester_package) = registry.get(&requester.package)
        && requester_package.has_skill(skill)
    {
        let constraint = primary_package_constraint(constraints, &requester.package)
            .unwrap_or(&requester_package.constraint)
            .clone();
        return Ok(PendingItem {
            package: requester.package.clone(),
            item: skill.clone(),
            kind: ItemKind::Skill,
            constraint,
            required_by,
            is_local: requester_package.is_local,
            spec: requester_package.spec.clone(),
        });
    }

    for (package_name, package) in registry {
        if package_name == &requester.package {
            continue;
        }
        if !package.has_skill(skill) {
            continue;
        }

        let constraint = primary_package_constraint(constraints, package_name)
            .unwrap_or(&package.constraint)
            .clone();
        return Ok(PendingItem {
            package: package_name.clone(),
            item: skill.clone(),
            kind: ItemKind::Skill,
            constraint,
            required_by: required_by.clone(),
            is_local: package.is_local,
            spec: package.spec.clone(),
        });
    }

    let mut searched: Vec<String> = registry.keys().map(ToString::to_string).collect();
    searched.sort();
    Err(ResolutionError::SkillNotFound {
        skill: skill.to_string(),
        required_by,
        searched,
    }
    .into())
}

pub(crate) fn primary_package_constraint<'a>(
    constraints: &'a HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    package: &SourceName,
) -> Option<&'a VersionConstraint> {
    constraints
        .get(package)
        .and_then(|entries| entries.first().map(|(_, constraint)| constraint))
}
