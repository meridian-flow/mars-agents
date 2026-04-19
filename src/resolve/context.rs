use std::collections::HashMap;

use indexmap::IndexMap;

use super::filter::push_filter_constraint;
use super::{
    PackageResolutionState, PackageVersions, PendingItem, RegisteredPackage, ResolvedGraph,
    ResolvedNode, VersionConstraint, VisitedSet,
};
use crate::config::FilterMode;
use crate::types::{SourceId, SourceName};

/// Mutable resolver state threaded through bottom-up resolution and DFS traversal.
pub struct ResolverContext {
    registry: IndexMap<SourceName, RegisteredPackage>,
    package_states: HashMap<SourceName, PackageResolutionState>,
    id_index: HashMap<SourceId, SourceName>,
    version_constraints: HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    materialization_filters: HashMap<SourceName, Vec<FilterMode>>,
    stack: Vec<PendingItem>,
    visited: VisitedSet,
    package_versions: PackageVersions,
}

impl ResolverContext {
    pub fn new() -> Self {
        Self {
            registry: IndexMap::new(),
            package_states: HashMap::new(),
            id_index: HashMap::new(),
            version_constraints: HashMap::new(),
            materialization_filters: HashMap::new(),
            stack: Vec::new(),
            visited: VisitedSet::new(),
            package_versions: PackageVersions::new(),
        }
    }

    pub(super) fn registry(&self) -> &IndexMap<SourceName, RegisteredPackage> {
        &self.registry
    }

    pub(super) fn registry_mut(&mut self) -> &mut IndexMap<SourceName, RegisteredPackage> {
        &mut self.registry
    }

    pub(super) fn package_states(&self) -> &HashMap<SourceName, PackageResolutionState> {
        &self.package_states
    }

    pub(super) fn package_states_mut(
        &mut self,
    ) -> &mut HashMap<SourceName, PackageResolutionState> {
        &mut self.package_states
    }

    pub(super) fn id_index(&self) -> &HashMap<SourceId, SourceName> {
        &self.id_index
    }

    pub(super) fn id_index_mut(&mut self) -> &mut HashMap<SourceId, SourceName> {
        &mut self.id_index
    }

    pub(super) fn version_constraints(
        &self,
    ) -> &HashMap<SourceName, Vec<(String, VersionConstraint)>> {
        &self.version_constraints
    }

    pub(super) fn materialization_filters(&self) -> &HashMap<SourceName, Vec<FilterMode>> {
        &self.materialization_filters
    }

    pub(super) fn visited(&self) -> &VisitedSet {
        &self.visited
    }

    pub(super) fn visited_mut(&mut self) -> &mut VisitedSet {
        &mut self.visited
    }

    pub(super) fn package_versions_mut(&mut self) -> &mut PackageVersions {
        &mut self.package_versions
    }

    pub fn add_version_constraint(
        &mut self,
        package: &SourceName,
        requester: &str,
        constraint: VersionConstraint,
    ) {
        self.version_constraints
            .entry(package.clone())
            .or_default()
            .push((requester.to_string(), constraint));
    }

    pub fn add_filter(&mut self, package: &SourceName, filter: FilterMode) {
        push_filter_constraint(&mut self.materialization_filters, package, &filter);
    }

    pub fn push_pending(&mut self, item: PendingItem) {
        self.stack.push(item);
    }

    pub fn pop_pending(&mut self) -> Option<PendingItem> {
        self.stack.pop()
    }

    pub fn into_graph(self) -> ResolvedGraph {
        let mut nodes: IndexMap<SourceName, ResolvedNode> = IndexMap::new();
        for (name, package) in self.registry {
            nodes.insert(name, package.node);
        }

        let mut order: Vec<SourceName> = nodes.keys().cloned().collect();
        order.sort();

        ResolvedGraph {
            nodes,
            order,
            id_index: self.id_index,
            filters: self.materialization_filters,
        }
    }
}
