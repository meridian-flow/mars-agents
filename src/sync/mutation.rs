//! Config mutation logic for the sync pipeline.
//!
//! Handles applying mutations to `mars.toml` and `mars.local.toml` under the sync lock.

use std::path::PathBuf;

use crate::config::{Config, DependencyEntry, FilterConfig, LocalConfig, OverrideEntry};
use crate::error::{ConfigError, MarsError};
use crate::types::{ItemName, RenameMap, SourceName};

/// Config mutation to apply atomically under flock.
#[derive(Debug, Clone)]
pub enum ConfigMutation {
    /// Add or update a dependency in mars.toml.
    UpsertDependency {
        name: SourceName,
        entry: DependencyEntry,
    },
    /// Add or update multiple dependencies in mars.toml atomically under one sync lock.
    BatchUpsert(Vec<(SourceName, DependencyEntry)>),
    /// Remove a dependency from mars.toml.
    RemoveDependency { name: SourceName },
    /// Add or update an override in mars.local.toml.
    SetOverride {
        source_name: SourceName,
        local_path: PathBuf,
    },
    /// Remove an override from mars.local.toml.
    ClearOverride { source_name: SourceName },
    /// Set or update a rename mapping for one managed item.
    SetRename {
        source_name: SourceName,
        from: String,
        to: String,
    },
}

/// Metadata captured when `UpsertDependency` mutates an existing/new dependency.
#[derive(Debug, Clone)]
pub struct DependencyUpsertChange {
    pub name: SourceName,
    pub already_exists: bool,
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    pub old_filter: Option<FilterConfig>,
    pub new_filter: FilterConfig,
}

/// Apply a config mutation to the in-memory config.
///
/// Public so that CLI commands can batch mutations before triggering sync.
pub fn apply_config_mutation(
    config: &mut Config,
    mutation: &ConfigMutation,
) -> Result<(), MarsError> {
    apply_mutation(config, mutation).map(|_| ())
}

pub(crate) fn apply_mutation(
    config: &mut Config,
    mutation: &ConfigMutation,
) -> Result<Vec<DependencyUpsertChange>, MarsError> {
    match mutation {
        ConfigMutation::UpsertDependency { name, entry } => {
            Ok(vec![apply_dependency_upsert(config, name, entry)])
        }
        ConfigMutation::BatchUpsert(entries) => {
            let mut changes = Vec::with_capacity(entries.len());
            for (name, entry) in entries {
                changes.push(apply_dependency_upsert(config, name, entry));
            }
            Ok(changes)
        }
        ConfigMutation::RemoveDependency { name } => {
            if !config.dependencies.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.to_string(),
                    message: format!("dependency `{name}` not found in mars.toml"),
                });
            }
            config.dependencies.shift_remove(name);
            Ok(Vec::new())
        }
        ConfigMutation::SetOverride { source_name, .. } => {
            if !config.dependencies.contains_key(source_name) {
                return Err(MarsError::Source {
                    source_name: source_name.to_string(),
                    message: format!("dependency `{source_name}` not found in mars.toml"),
                });
            }
            Ok(Vec::new())
        }
        ConfigMutation::SetRename {
            source_name,
            from,
            to,
        } => {
            let dep =
                config
                    .dependencies
                    .get_mut(source_name)
                    .ok_or_else(|| MarsError::Source {
                        source_name: source_name.to_string(),
                        message: format!("dependency `{source_name}` not found in mars.toml"),
                    })?;
            let rename_map = dep.filter.rename.get_or_insert_with(RenameMap::new);
            rename_map.insert(ItemName::from(from.as_str()), ItemName::from(to.as_str()));
            Ok(Vec::new())
        }
        ConfigMutation::ClearOverride { .. } => Ok(Vec::new()),
    }
}

pub(crate) fn apply_local_mutation(local: &mut LocalConfig, mutation: &ConfigMutation) {
    match mutation {
        ConfigMutation::SetOverride {
            source_name,
            local_path,
        } => {
            local.overrides.insert(
                source_name.clone(),
                OverrideEntry {
                    path: local_path.clone(),
                },
            );
        }
        ConfigMutation::ClearOverride { source_name } => {
            local.overrides.shift_remove(source_name);
        }
        ConfigMutation::UpsertDependency { .. }
        | ConfigMutation::BatchUpsert(..)
        | ConfigMutation::RemoveDependency { .. }
        | ConfigMutation::SetRename { .. } => {}
    }
}

fn apply_dependency_upsert(
    config: &mut Config,
    name: &SourceName,
    entry: &DependencyEntry,
) -> DependencyUpsertChange {
    if let Some(existing) = config.dependencies.get_mut(name) {
        let old_version = existing.version.clone();
        let old_filter = existing.filter.clone();

        // Merge: update location fields, preserve user customizations
        existing.url = entry.url.clone();
        existing.path = entry.path.clone();
        existing.version = entry.version.clone();
        // Atomic filter replacement: when any filter field is set on the
        // incoming entry, replace the entire filter config (minus rename).
        // This prevents mixed-mode states like agents + only_skills.
        // When no filter flags are provided (e.g., version bump), preserve existing.
        if entry.filter.has_any_filter() {
            let rename = existing.filter.rename.take();
            existing.filter = entry.filter.clone();
            // Preserve rename — those are set via `mars rename`, not `mars add`
            existing.filter.rename = rename;
        }
        // Never overwrite rename rules from add — those are set via `mars rename`

        DependencyUpsertChange {
            name: name.clone(),
            already_exists: true,
            old_version,
            new_version: existing.version.clone(),
            old_filter: Some(old_filter),
            new_filter: existing.filter.clone(),
        }
    } else {
        config.dependencies.insert(name.clone(), entry.clone());
        DependencyUpsertChange {
            name: name.clone(),
            already_exists: false,
            old_version: None,
            new_version: entry.version.clone(),
            old_filter: None,
            new_filter: entry.filter.clone(),
        }
    }
}

pub(crate) fn is_config_not_found(error: &MarsError) -> bool {
    matches!(error, MarsError::Config(ConfigError::NotFound { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_mutation_atomic_filter_replacement() {
        let mut config = Config::default();
        // First add with agents filter
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["reviewer".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();
        assert!(config.dependencies["base"].filter.agents.is_some());

        // Re-add with only_skills — should atomically replace, clearing agents
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert!(dep.filter.only_skills);
        assert!(
            dep.filter.agents.is_none(),
            "agents should be cleared by atomic replacement"
        );
    }

    #[test]
    fn apply_mutation_preserves_filters_on_version_bump() {
        let mut config = Config::default();
        // Add with agents filter
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["coder".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        // Re-add with no filter (version bump only)
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v2".into()),
            filter: FilterConfig::default(),
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert_eq!(dep.version.as_deref(), Some("v2"));
        assert_eq!(
            dep.filter.agents.as_deref(),
            Some(&["coder".into()][..]),
            "agents filter should be preserved on version bump"
        );
    }

    #[test]
    fn apply_mutation_preserves_rename_on_filter_change() {
        let mut config = Config::default();
        let mut rename_map = RenameMap::new();
        rename_map.insert("old".into(), "new".into());

        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: None,
            filter: FilterConfig {
                agents: Some(vec!["coder".into()]),
                rename: Some(rename_map),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        // Re-add with different filter — rename should be preserved
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: None,
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert!(dep.filter.only_skills);
        assert!(dep.filter.agents.is_none());
        assert!(
            dep.filter.rename.is_some(),
            "rename should be preserved across filter changes"
        );
        assert_eq!(
            dep.filter.rename.as_ref().unwrap().get("old").unwrap(),
            "new"
        );
    }

    #[test]
    fn apply_mutation_batch_upsert_applies_all_entries() {
        let mut config = Config::default();
        let batch = vec![
            (
                "base".into(),
                DependencyEntry {
                    url: Some("https://github.com/org/base.git".into()),
                    path: None,
                    version: Some("v1".into()),
                    filter: FilterConfig::default(),
                },
            ),
            (
                "workflow".into(),
                DependencyEntry {
                    url: Some("https://github.com/org/workflow.git".into()),
                    path: None,
                    version: Some("v2".into()),
                    filter: FilterConfig::default(),
                },
            ),
        ];

        let changes = apply_mutation(&mut config, &ConfigMutation::BatchUpsert(batch)).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(config.dependencies.contains_key("base"));
        assert!(config.dependencies.contains_key("workflow"));
    }

    #[test]
    fn apply_mutation_returns_old_and_new_filters_for_readd() {
        let mut config = Config::default();
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["reviewer".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v2".into()),
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        let changes = apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert!(change.already_exists);
        assert_eq!(change.name, "base");
        assert_eq!(
            change.old_filter.as_ref().and_then(|f| f.agents.as_deref()),
            Some(&["reviewer".into()][..])
        );
        assert!(change.new_filter.only_skills);
        assert!(change.new_filter.agents.is_none());
    }
}
