use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;
use semver::{Version, VersionReq};

use super::compat::CompatibilityResult;
use crate::config::{FilterMode, Manifest, SourceSpec};
use crate::error::ResolutionError;
use crate::lock::ItemKind;
use crate::source::ResolvedRef;
use crate::types::{ItemName, SourceId, SourceName};

/// The resolved dependency graph — all sources with concrete versions.
///
/// Produced by the resolver after fetching sources, reading manifests,
/// intersecting version constraints, and deterministic ordering.
#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    pub nodes: IndexMap<SourceName, ResolvedNode>,
    /// Deterministic alphabetical order (prompt packages don't require dependency ordering).
    pub order: Vec<SourceName>,
    pub id_index: HashMap<SourceId, SourceName>,
    /// All filter constraints collected for each source (direct + transitive).
    pub filters: HashMap<SourceName, Vec<FilterMode>>,
}

/// A single node in the resolved graph.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub source_name: SourceName,
    pub source_id: SourceId,
    pub rooted_ref: RootedSourceRef,
    pub resolved_ref: ResolvedRef,
    pub latest_version: Option<Version>,
    /// None if source has no mars.toml.
    pub manifest: Option<Manifest>,
    /// Source names this depends on.
    pub deps: Vec<SourceName>,
}

/// Source checkout provenance and rooted package boundary.
#[derive(Debug, Clone)]
pub struct RootedSourceRef {
    pub checkout_root: PathBuf,
    pub package_root: PathBuf,
}

/// How a version constraint was specified.
#[derive(Debug, Clone)]
pub enum VersionConstraint {
    /// Semver requirement (^1.0, >=0.5.0, ~2.1, exact version).
    Semver(VersionReq),
    /// Any version, prefer newest.
    Latest,
    /// Branch or commit pin — no semver resolution.
    RefPin(String),
}

impl std::fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionConstraint::Semver(req) => write!(f, "{req}"),
            VersionConstraint::Latest => write!(f, "latest"),
            VersionConstraint::RefPin(reference) => write!(f, "ref:{reference}"),
        }
    }
}

/// An item waiting to be processed in DFS traversal.
#[derive(Debug, Clone)]
pub struct PendingItem {
    /// Package containing this item.
    pub package: SourceName,
    /// Item name.
    pub item: ItemName,
    /// Agent or Skill.
    pub kind: ItemKind,
    /// Version constraint from config.
    pub constraint: VersionConstraint,
    /// Who requested this item (for error context).
    pub required_by: String,
    /// True if from a local path dependency (skip version checks).
    pub is_local: bool,
    /// Source spec for fetching if not already in registry.
    pub spec: SourceSpec,
}

/// Result of checking whether an item was seen already.
#[derive(Debug)]
pub enum VersionCheckResult {
    /// Item has not been visited yet.
    NotSeen,
    /// Item was visited with a compatible version.
    SameVersion,
    /// Item was visited with a potentially conflicting version (latest vs pinned).
    PotentiallyConflicting {
        existing: VersionConstraint,
        requested: VersionConstraint,
    },
    /// Item was visited with a conflicting version.
    DifferentVersion {
        existing: VersionConstraint,
        requested: VersionConstraint,
    },
}

/// Stable key for visited items.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct VisitedItem {
    package: SourceName,
    item: ItemName,
}

/// Stored version information for a visited item.
#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub constraint: VersionConstraint,
    pub resolved_ref: ResolvedRef,
}

/// Tracks visited items with version-aware lookup for DFS traversal.
pub struct VisitedSet {
    /// Fast lookup by (package, item).
    index: HashMap<(SourceName, ItemName), ResolvedVersion>,
}

impl VisitedSet {
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
        }
    }

    fn index_key(package: &SourceName, item: &ItemName) -> (SourceName, ItemName) {
        let key = VisitedItem {
            package: package.clone(),
            item: item.clone(),
        };
        (key.package, key.item)
    }

    /// Check whether an item was visited and compare version constraints.
    pub fn check_version(
        &self,
        package: &SourceName,
        item: &ItemName,
        constraint: &VersionConstraint,
    ) -> VersionCheckResult {
        match self.index.get(&Self::index_key(package, item)) {
            None => VersionCheckResult::NotSeen,
            Some(existing) => match existing.constraint.compatible_with_resolved(
                constraint,
                existing.resolved_ref.version.as_ref(),
            ) {
                CompatibilityResult::Compatible => VersionCheckResult::SameVersion,
                CompatibilityResult::PotentiallyConflicting => {
                    VersionCheckResult::PotentiallyConflicting {
                        existing: existing.constraint.clone(),
                        requested: constraint.clone(),
                    }
                }
                CompatibilityResult::Conflicting => VersionCheckResult::DifferentVersion {
                    existing: existing.constraint.clone(),
                    requested: constraint.clone(),
                },
            },
        }
    }

    /// Insert an item as visited.
    pub fn insert(
        &mut self,
        package: SourceName,
        item: ItemName,
        constraint: VersionConstraint,
        resolved_ref: ResolvedRef,
    ) {
        self.index.insert(
            Self::index_key(&package, &item),
            ResolvedVersion {
                constraint,
                resolved_ref,
            },
        );
    }

    /// Iterate all visited items for graph/output assembly.
    pub fn iter(&self) -> impl Iterator<Item = (&(SourceName, ItemName), &ResolvedVersion)> {
        self.index.iter()
    }
}

/// Tracks resolved version per package and rejects divergent refs.
pub struct PackageVersions {
    /// package -> (resolved_ref, first_constraint, first_required_by)
    versions: HashMap<SourceName, (ResolvedRef, VersionConstraint, String)>,
}

impl PackageVersions {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
        }
    }

    /// Check existing package version or insert if first time seen.
    pub fn check_or_insert(
        &mut self,
        package: &SourceName,
        resolved: &ResolvedRef,
        requested: &VersionConstraint,
        required_by: &str,
        is_local: bool,
    ) -> Result<(), ResolutionError> {
        if is_local {
            return Ok(());
        }

        match self.versions.entry(package.clone()) {
            Entry::Vacant(entry) => {
                entry.insert((resolved.clone(), requested.clone(), required_by.to_string()));
                Ok(())
            }
            Entry::Occupied(entry) => {
                let (existing_ref, existing_constraint, existing_by) = entry.get();
                match existing_constraint.compatible_with_resolved(
                    requested,
                    existing_ref.version.as_ref().or(resolved.version.as_ref()),
                ) {
                    CompatibilityResult::Compatible
                    | CompatibilityResult::PotentiallyConflicting => {
                        if resolved_ref_matches(existing_ref, resolved) {
                            Ok(())
                        } else {
                            Err(ResolutionError::PackageVersionConflict {
                                package: package.to_string(),
                                existing: format!("{existing_ref:?} (required by {existing_by})"),
                                requested: format!("{resolved:?} (required by {required_by})"),
                                chain: required_by.to_string(),
                            })
                        }
                    }
                    CompatibilityResult::Conflicting => {
                        Err(ResolutionError::PackageVersionConflict {
                            package: package.to_string(),
                            existing: format!("{existing_constraint} (required by {existing_by})"),
                            requested: format!("{requested} (required by {required_by})"),
                            chain: required_by.to_string(),
                        })
                    }
                }
            }
        }
    }
}

fn resolved_ref_matches(existing: &ResolvedRef, incoming: &ResolvedRef) -> bool {
    existing.source_name == incoming.source_name
        && existing.version == incoming.version
        && existing.version_tag == incoming.version_tag
        && existing.commit == incoming.commit
        && existing.tree_path == incoming.tree_path
}

/// Options controlling resolution behavior.
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// If true, prefer newest version instead of minimum (for `mars upgrade`).
    pub maximize: bool,
    /// Source names to upgrade (empty = all, when maximize=true).
    pub upgrade_targets: HashSet<SourceName>,
    /// If true, treat direct dependency constraints for upgrade targets as
    /// unconstrained during resolution (used by `mars upgrade --bump`).
    pub bump_direct_constraints: bool,
    /// If true, locked commit replay failures become hard errors.
    pub frozen: bool,
}
