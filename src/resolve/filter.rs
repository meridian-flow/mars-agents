use std::collections::HashMap;

use indexmap::IndexMap;

use crate::config::FilterMode;
use crate::lock::ItemKind;
use crate::types::{ItemName, SourceName};

use super::package::RegisteredPackage;

pub(crate) fn is_item_excluded(
    filter_constraints: &HashMap<SourceName, Vec<FilterMode>>,
    registry: &IndexMap<SourceName, RegisteredPackage>,
    package: &SourceName,
    kind: ItemKind,
    item: &ItemName,
) -> bool {
    let source_path = registry
        .get(package)
        .and_then(|pkg| pkg.item(kind, item))
        .map(|discovered| discovered.source_path.to_string_lossy().into_owned());

    filter_constraints
        .get(package)
        .map(|filters| {
            filters.iter().any(|filter| match filter {
                FilterMode::Exclude(excluded) => excluded.iter().any(|excluded_item| {
                    excluded_item == item
                        || source_path
                            .as_deref()
                            .is_some_and(|path| excluded_item.as_ref() == path)
                }),
                _ => false,
            })
        })
        .unwrap_or(false)
}

pub(crate) fn push_filter_constraint(
    constraints: &mut HashMap<SourceName, Vec<FilterMode>>,
    source_name: &SourceName,
    filter: &FilterMode,
) {
    let entry = constraints.entry(source_name.clone()).or_default();
    if !entry.contains(filter) {
        entry.push(filter.clone());
    }
}

pub(crate) fn is_unfiltered_request(filter: &FilterMode) -> bool {
    matches!(filter, FilterMode::All)
}
