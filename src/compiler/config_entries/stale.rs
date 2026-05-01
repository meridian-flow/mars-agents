//! Stale config-entry cleanup.

use std::collections::BTreeMap;

use crate::lock::ConfigEntryRecord;

/// Find config entry keys that exist in previous lock state but not in current.
pub fn find_stale_entries(
    previous: &BTreeMap<String, BTreeMap<String, ConfigEntryRecord>>,
    current: &BTreeMap<String, BTreeMap<String, ConfigEntryRecord>>,
) -> BTreeMap<String, Vec<String>> {
    let mut stale = BTreeMap::new();

    for (target_root, previous_entries) in previous {
        let current_entries = current.get(target_root);
        let stale_keys: Vec<String> = previous_entries
            .keys()
            .filter(|key| current_entries.is_none_or(|entries| !entries.contains_key(*key)))
            .cloned()
            .collect();

        if !stale_keys.is_empty() {
            stale.insert(target_root.clone(), stale_keys);
        }
    }

    stale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(source: &str) -> ConfigEntryRecord {
        ConfigEntryRecord {
            source: source.to_string(),
        }
    }

    #[test]
    fn finds_entries_missing_from_current_target() {
        let previous = BTreeMap::from([(
            ".claude".to_string(),
            BTreeMap::from([
                ("mcp:old".to_string(), record("base")),
                ("mcp:kept".to_string(), record("base")),
            ]),
        )]);
        let current = BTreeMap::from([(
            ".claude".to_string(),
            BTreeMap::from([("mcp:kept".to_string(), record("base"))]),
        )]);

        let stale = find_stale_entries(&previous, &current);

        assert_eq!(stale[".claude"], vec!["mcp:old".to_string()]);
    }
}
