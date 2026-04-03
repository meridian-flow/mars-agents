use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, MarsError};
use crate::types::{ItemName, RenameMap, SourceId, SourceName, SourceUrl};

/// Top-level mars.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageInfo>,
    #[serde(default)]
    pub dependencies: IndexMap<SourceName, DependencyEntry>,
    #[serde(default)]
    pub settings: Settings,
}

/// Package metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Unified dependency specification — replaces both old DepSpec and SourceEntry.
/// Used in [dependencies] for both "what to install locally" (consumer)
/// and "what downstream consumers inherit" (package manifest).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DependencyEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<SourceUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(flatten)]
    pub filter: FilterConfig,
}

/// Source-manifest view extracted from mars.toml.
///
/// In source repositories, `mars.toml` may include `[package]` +
/// `[dependencies]` only, or coexist with consumer sections.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub dependencies: IndexMap<String, DependencyEntry>,
}

/// Shared include/exclude/rename filter configuration for a source.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FilterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<ItemName>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<ItemName>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<ItemName>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<RenameMap>,
}

/// Dev override config (mars.local.toml).
///
/// Gitignored — each developer can work with local checkouts while
/// production config points at git.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LocalConfig {
    #[serde(default)]
    pub overrides: IndexMap<SourceName, OverrideEntry>,
}

/// Dev override — local path swap for a git source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverrideEntry {
    pub path: PathBuf,
}

/// Global settings — extensible via additional fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Custom managed output directory (e.g. ".claude"). Default: ".agents".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_root: Option<String>,
    /// Directories to symlink agents/ and skills/ into (e.g. [".claude"]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

/// Resolved source specification after merging config and overrides.
#[derive(Debug, Clone)]
pub enum SourceSpec {
    Git(GitSpec),
    Path(PathBuf),
}

/// Git source specification preserved when overrides are active.
#[derive(Debug, Clone)]
pub struct GitSpec {
    pub url: SourceUrl,
    pub version: Option<String>,
}

/// How items are filtered from a source.
#[derive(Debug, Clone)]
pub enum FilterMode {
    /// Install everything from the source.
    All,
    /// Only install specific agents and/or skills.
    Include {
        agents: Vec<ItemName>,
        skills: Vec<ItemName>,
    },
    /// Install everything except these items.
    Exclude(Vec<ItemName>),
}

/// Effective configuration after merging mars.toml and mars.local.toml.
///
/// This is what the rest of the pipeline operates on.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub sources: IndexMap<SourceName, EffectiveSource>,
    pub settings: Settings,
}

/// A fully-resolved source with override tracking.
#[derive(Debug, Clone)]
pub struct EffectiveSource {
    pub name: SourceName,
    pub id: SourceId,
    pub spec: SourceSpec,
    pub filter: FilterMode,
    pub rename: RenameMap,
    pub is_overridden: bool,
    pub original_git: Option<GitSpec>,
}

const CONFIG_FILE: &str = "mars.toml";
const LOCAL_CONFIG_FILE: &str = "mars.local.toml";

/// Load mars.toml from the given root directory.
pub fn load(root: &Path) -> Result<Config, MarsError> {
    let path = root.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NotFound { path: path.clone() }
        } else {
            ConfigError::Io(e)
        }
    })?;
    let mut config: Config = toml::from_str(&content).map_err(ConfigError::Parse)?;
    migrate_legacy_source_urls(&mut config);
    Ok(config)
}

/// Load source manifest data from mars.toml in a source tree root.
///
/// Returns `None` when mars.toml is absent or when it has no `[package]`
/// section (consumer config only).
pub fn load_manifest(source_root: &Path) -> Result<Option<Manifest>, MarsError> {
    let path = source_root.join(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let parsed: Config =
                toml::from_str(&content).map_err(|e| crate::error::ConfigError::Invalid {
                    message: format!("failed to parse {}: {e}", path.display()),
                })?;
            let Some(package) = parsed.package else {
                return Ok(None);
            };
            // For manifest purposes, filter to only deps with url+version (package deps)
            let deps: IndexMap<String, DependencyEntry> = parsed
                .dependencies
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            Ok(Some(Manifest {
                package,
                dependencies: deps,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MarsError::Io(e)),
    }
}

/// Load mars.local.toml (returns Default if absent).
pub fn load_local(root: &Path) -> Result<LocalConfig, MarsError> {
    let path = root.join(LOCAL_CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let local: LocalConfig = toml::from_str(&content).map_err(ConfigError::Parse)?;
            Ok(local)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalConfig::default()),
        Err(e) => Err(ConfigError::Io(e).into()),
    }
}

/// Merge config + local overrides into EffectiveConfig.
///
/// Validates:
/// - Each source has `url` XOR `path` (not both, not neither)
/// - Each source uses either include filters (`agents`/`skills`) or `exclude`, not both
/// - Warns (via eprintln) if an override references a source name not in config
pub fn merge(config: Config, local: LocalConfig) -> Result<EffectiveConfig, MarsError> {
    merge_with_root(config, local, Path::new("."))
}

/// Same as `merge`, but uses an explicit root for path-based SourceId canonicalization.
pub fn merge_with_root(
    config: Config,
    local: LocalConfig,
    root: &Path,
) -> Result<EffectiveConfig, MarsError> {
    let mut sources = IndexMap::new();

    for (name, entry) in &config.dependencies {
        // Reject reserved name
        if name.as_ref() == "_self" {
            return Err(ConfigError::Invalid {
                message: "dependency name `_self` is reserved for local package items".into(),
            }
            .into());
        }

        // Validate url XOR path
        let base_spec = match (&entry.url, &entry.path) {
            (Some(url), None) => SourceSpec::Git(GitSpec {
                url: url.clone(),
                version: entry.version.clone(),
            }),
            (None, Some(path)) => SourceSpec::Path(path.clone()),
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid {
                    message: format!("source `{name}` has both `url` and `path` — pick one"),
                }
                .into());
            }
            (None, None) => {
                return Err(ConfigError::Invalid {
                    message: format!(
                        "source `{name}` has neither `url` nor `path` — one is required"
                    ),
                }
                .into());
            }
        };

        // Validate filter mode: agents/skills XOR exclude
        let has_include = entry.filter.agents.is_some() || entry.filter.skills.is_some();
        let has_exclude = entry.filter.exclude.is_some();
        if has_include && has_exclude {
            return Err(ConfigError::ConflictingFilters {
                name: name.to_string(),
            }
            .into());
        }

        let filter = if has_include {
            FilterMode::Include {
                agents: entry.filter.agents.clone().unwrap_or_default(),
                skills: entry.filter.skills.clone().unwrap_or_default(),
            }
        } else if has_exclude {
            FilterMode::Exclude(entry.filter.exclude.clone().unwrap_or_default())
        } else {
            FilterMode::All
        };

        let rename = entry.filter.rename.clone().unwrap_or_default();

        // Check if this source has a local override
        let (spec, is_overridden, original_git) = if let Some(ov) = local.overrides.get(name) {
            let original = match &base_spec {
                SourceSpec::Git(git) => Some(git.clone()),
                SourceSpec::Path(_) => None,
            };
            (SourceSpec::Path(ov.path.clone()), true, original)
        } else {
            (base_spec, false, None)
        };
        let id = source_id_for_spec(root, &spec);

        sources.insert(
            name.clone(),
            EffectiveSource {
                name: name.clone(),
                id,
                spec,
                filter,
                rename,
                is_overridden,
                original_git,
            },
        );
    }

    // Warn if override references a dependency not in config
    for override_name in local.overrides.keys() {
        if !config.dependencies.contains_key(override_name) {
            eprintln!(
                "warning: override `{override_name}` references a dependency not in mars.toml"
            );
        }
    }

    Ok(EffectiveConfig {
        sources,
        settings: config.settings,
    })
}

fn source_id_for_spec(root: &Path, spec: &SourceSpec) -> SourceId {
    match spec {
        SourceSpec::Git(git) => SourceId::git(git.url.clone()),
        SourceSpec::Path(path) => match SourceId::path(root, path) {
            Ok(id) => id,
            Err(_) => {
                let canonical = if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                };
                SourceId::Path { canonical }
            }
        },
    }
}

fn migrate_legacy_source_urls(config: &mut Config) {
    for dep in config.dependencies.values_mut() {
        if let Some(url) = dep.url.as_mut() {
            let raw = url.as_str();
            if should_upgrade_legacy_git_url(raw) {
                *url = SourceUrl::from(format!("https://{raw}"));
            }
        }
    }
}

fn should_upgrade_legacy_git_url(url: &str) -> bool {
    !url.contains("://") && !url.starts_with("git@") && url.contains('/') && url.contains('.')
}

/// Write mars.toml atomically.
pub fn save(root: &Path, config: &Config) -> Result<(), MarsError> {
    let path = root.join(CONFIG_FILE);
    let content = toml::to_string_pretty(config).map_err(|e| ConfigError::Invalid {
        message: format!("failed to serialize config: {e}"),
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())
}

/// Write mars.local.toml atomically.
pub fn save_local(root: &Path, local: &LocalConfig) -> Result<(), MarsError> {
    let path = root.join(LOCAL_CONFIG_FILE);
    let content = toml::to_string_pretty(local).map_err(|e| ConfigError::Invalid {
        message: format!("failed to serialize local config: {e}"),
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_git_dependency() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.dependencies.len(), 1);
        let entry = &config.dependencies["base"];
        assert_eq!(
            entry.url.as_deref(),
            Some("https://github.com/org/base.git")
        );
        assert!(entry.path.is_none());
        assert_eq!(entry.version.as_deref(), Some("v1.0"));
    }

    #[test]
    fn parse_path_dependency() {
        let toml_str = r#"
[dependencies.local]
path = "../my-agents"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let entry = &config.dependencies["local"];
        assert!(entry.url.is_none());
        assert_eq!(entry.path.as_deref(), Some(Path::new("../my-agents")));
    }

    #[test]
    fn parse_mixed_dependencies() {
        let toml_str = r#"
[dependencies.remote]
url = "https://github.com/org/remote.git"
version = "v2.0"
agents = ["coder", "reviewer"]

[dependencies.local]
path = "/home/dev/agents"
exclude = ["experimental"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.dependencies.len(), 2);
        assert!(config.dependencies.contains_key("remote"));
        assert!(config.dependencies.contains_key("local"));
    }

    #[test]
    fn parse_package_and_dependencies_coexist() {
        let toml_str = r#"
[package]
name = "my-agents"
version = "0.1.0"

[dependencies.base]
url = "https://github.com/org/base.git"
version = ">=1.0.0"

[dependencies.local]
path = "../local-agents"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.package.is_some());
        assert!(config.dependencies.contains_key("base"));
        assert!(config.dependencies.contains_key("local"));
    }

    #[test]
    fn parse_include_filter() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
agents = ["coder"]
skills = ["review"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();
        let source = &effective.sources["base"];
        match &source.filter {
            FilterMode::Include { agents, skills } => {
                assert_eq!(agents, &["coder"]);
                assert_eq!(skills, &["review"]);
            }
            other => panic!("expected Include, got {other:?}"),
        }
    }

    #[test]
    fn parse_exclude_filter() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
exclude = ["experimental", "deprecated"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();
        let source = &effective.sources["base"];
        match &source.filter {
            FilterMode::Exclude(items) => {
                assert_eq!(items, &["experimental", "deprecated"]);
            }
            other => panic!("expected Exclude, got {other:?}"),
        }
    }

    #[test]
    fn error_on_both_include_and_exclude() {
        let toml_str = r#"
[dependencies.bad]
url = "https://github.com/org/bad.git"
agents = ["coder"]
exclude = ["reviewer"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bad"),
            "error should mention dependency name: {err}"
        );
    }

    #[test]
    fn error_on_neither_url_nor_path() {
        let toml_str = r#"
[dependencies.empty]
version = "v1.0"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("neither"),
            "error should mention 'neither': {err}"
        );
    }

    #[test]
    fn error_on_both_url_and_path() {
        let toml_str = r#"
[dependencies.both]
url = "https://github.com/org/repo.git"
path = "/local/path"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("both"), "error should mention 'both': {err}");
    }

    #[test]
    fn roundtrip_config() {
        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        version: Some("v1.0".into()),
                        filter: FilterConfig {
                            agents: Some(vec!["coder".into()]),
                            skills: None,
                            exclude: None,
                            rename: None,
                        },
                    },
                );
                m.insert(
                    "local".into(),
                    DependencyEntry {
                        url: None,
                        path: Some(PathBuf::from("../my-agents")),
                        version: None,
                        filter: FilterConfig::default(),
                    },
                );
                m
            },
            settings: Settings::default(),
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn load_from_disk() {
        let dir = TempDir::new().unwrap();
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0"
"#;
        std::fs::write(dir.path().join("mars.toml"), toml_str).unwrap();
        let config = load(dir.path()).unwrap();
        assert_eq!(config.dependencies.len(), 1);
    }

    #[test]
    fn load_migrates_legacy_bare_domain_url() {
        let dir = TempDir::new().unwrap();
        let toml_str = r#"
[dependencies.base]
url = "github.com/org/base"
"#;
        std::fs::write(dir.path().join("mars.toml"), toml_str).unwrap();

        let config = load(dir.path()).unwrap();
        assert_eq!(
            config.dependencies["base"].url.as_deref(),
            Some("https://github.com/org/base")
        );
    }

    #[test]
    fn load_does_not_migrate_ssh_url() {
        let dir = TempDir::new().unwrap();
        let toml_str = r#"
[dependencies.base]
url = "git@github.com:org/base.git"
"#;
        std::fs::write(dir.path().join("mars.toml"), toml_str).unwrap();

        let config = load(dir.path()).unwrap();
        assert_eq!(
            config.dependencies["base"].url.as_deref(),
            Some("git@github.com:org/base.git")
        );
    }

    #[test]
    fn load_missing_file_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let result = load(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "should be NotFound: {err}");
    }

    #[test]
    fn load_manifest_returns_none_without_package() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[dependencies.base]
url = "https://github.com/org/base.git"
"#,
        )
        .unwrap();

        let manifest = load_manifest(dir.path()).unwrap();
        assert!(manifest.is_none());
    }

    #[test]
    fn load_manifest_returns_package_and_dependencies() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[package]
name = "pkg"
version = "1.2.3"

[dependencies.base]
url = "https://github.com/org/base.git"
version = ">=1.0.0"
"#,
        )
        .unwrap();

        let manifest = load_manifest(dir.path()).unwrap().unwrap();
        assert_eq!(manifest.package.name, "pkg");
        assert_eq!(manifest.package.version, "1.2.3");
        assert!(manifest.dependencies.contains_key("base"));
    }

    #[test]
    fn load_local_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let local = load_local(dir.path()).unwrap();
        assert!(local.overrides.is_empty());
    }

    #[test]
    fn load_local_from_disk() {
        let dir = TempDir::new().unwrap();
        let toml_str = r#"
[overrides.base]
path = "/home/dev/local-base"
"#;
        std::fs::write(dir.path().join("mars.local.toml"), toml_str).unwrap();
        let local = load_local(dir.path()).unwrap();
        assert_eq!(local.overrides.len(), 1);
        assert_eq!(
            local.overrides["base"].path,
            PathBuf::from("/home/dev/local-base")
        );
    }

    #[test]
    fn merge_with_empty_local() {
        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        version: Some("v1.0".into()),
                        filter: FilterConfig::default(),
                    },
                );
                m
            },
            settings: Settings::default(),
            ..Config::default()
        };
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();
        assert_eq!(effective.sources.len(), 1);
        let source = &effective.sources["base"];
        assert!(!source.is_overridden);
        assert!(source.original_git.is_none());
        match &source.spec {
            SourceSpec::Git(git) => {
                assert_eq!(git.url, "https://github.com/org/base.git");
                assert_eq!(git.version.as_deref(), Some("v1.0"));
            }
            SourceSpec::Path(_) => panic!("expected Git"),
        }
    }

    #[test]
    fn merge_override_replaces_with_path() {
        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        version: Some("v1.0".into()),
                        filter: FilterConfig::default(),
                    },
                );
                m
            },
            settings: Settings::default(),
            ..Config::default()
        };
        let local = LocalConfig {
            overrides: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    OverrideEntry {
                        path: PathBuf::from("/home/dev/local-base"),
                    },
                );
                m
            },
        };
        let effective = merge(config, local).unwrap();
        let source = &effective.sources["base"];
        assert!(source.is_overridden);

        match &source.spec {
            SourceSpec::Path(p) => assert_eq!(p, &PathBuf::from("/home/dev/local-base")),
            SourceSpec::Git(_) => panic!("expected Path override"),
        }

        let orig = source.original_git.as_ref().unwrap();
        assert_eq!(orig.url, "https://github.com/org/base.git");
        assert_eq!(orig.version.as_deref(), Some("v1.0"));
    }

    #[test]
    fn merge_all_filter_mode() {
        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        version: None,
                        filter: FilterConfig::default(),
                    },
                );
                m
            },
            settings: Settings::default(),
            ..Config::default()
        };
        let effective = merge(config, LocalConfig::default()).unwrap();
        assert!(matches!(effective.sources["base"].filter, FilterMode::All));
    }

    #[test]
    fn save_and_reload() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        version: Some("v2.0".into()),
                        filter: FilterConfig::default(),
                    },
                );
                m
            },
            settings: Settings::default(),
            ..Config::default()
        };
        save(dir.path(), &config).unwrap();
        let reloaded = load(dir.path()).unwrap();
        assert_eq!(config, reloaded);
    }

    #[test]
    fn rename_map_preserved() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"

[dependencies.base.rename]
old-name = "new-name"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let effective = merge(config, LocalConfig::default()).unwrap();
        let source = &effective.sources["base"];
        assert_eq!(source.rename.get("old-name").unwrap(), "new-name");
    }

    #[test]
    fn self_dependency_name_rejected() {
        let toml_str = r#"
[dependencies._self]
url = "https://github.com/org/base.git"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("_self") && err.contains("reserved"),
            "should reject _self: {err}"
        );
    }

    #[test]
    fn managed_root_setting_roundtrip() {
        let config = Config {
            settings: Settings {
                managed_root: Some(".claude".into()),
                links: vec![],
            },
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.settings.managed_root.as_deref(),
            Some(".claude")
        );
    }
}
