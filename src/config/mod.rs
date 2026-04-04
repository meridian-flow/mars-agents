use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, MarsError};
use crate::models::ModelAlias;
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
    /// Model alias routing table: alias → {harness, model}.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub models: IndexMap<String, ModelAlias>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub only_skills: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub only_agents: bool,
}

fn is_false(v: &bool) -> bool {
    !v
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
    /// Install only skills, no agents.
    OnlySkills,
    /// Install only agents plus their transitive skill dependencies.
    OnlyAgents,
}

/// Effective configuration after merging mars.toml and mars.local.toml.
///
/// This is what the rest of the pipeline operates on.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub dependencies: IndexMap<SourceName, EffectiveDependency>,
    pub settings: Settings,
}

/// A fully-resolved source with override tracking.
#[derive(Debug, Clone)]
pub struct EffectiveDependency {
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
    let mut dependencies = IndexMap::new();

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

        // Validate filter combinations
        validate_filter(&entry.filter, name.as_ref())?;

        let filter = entry.filter.to_mode();

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

        dependencies.insert(
            name.clone(),
            EffectiveDependency {
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
        dependencies,
        settings: config.settings,
    })
}

/// Validate filter configuration for consistency.
///
/// Rejects invalid combinations:
/// - `only_skills` and `only_agents` together
/// - category-only flags with include lists
/// - category-only flags with exclude
/// - include lists with exclude
pub fn validate_filter(filter: &FilterConfig, dep_name: &str) -> Result<(), MarsError> {
    let has_include = filter.agents.is_some() || filter.skills.is_some();
    let has_exclude = filter.exclude.is_some();
    let has_category = filter.only_skills || filter.only_agents;

    if filter.only_skills && filter.only_agents {
        return Err(ConfigError::Invalid {
            message: format!(
                "dependency `{dep_name}`: only_skills and only_agents are mutually exclusive"
            ),
        }
        .into());
    }
    if has_category && has_include {
        return Err(ConfigError::Invalid {
            message: format!(
                "dependency `{dep_name}`: only_skills/only_agents cannot combine with agents/skills lists"
            ),
        }
        .into());
    }
    if has_category && has_exclude {
        return Err(ConfigError::Invalid {
            message: format!(
                "dependency `{dep_name}`: only_skills/only_agents cannot combine with exclude"
            ),
        }
        .into());
    }
    if has_include && has_exclude {
        return Err(ConfigError::ConflictingFilters {
            name: dep_name.to_string(),
        }
        .into());
    }
    Ok(())
}

impl FilterConfig {
    /// Convert to the resolved FilterMode enum.
    pub fn to_mode(&self) -> FilterMode {
        if self.only_skills {
            FilterMode::OnlySkills
        } else if self.only_agents {
            FilterMode::OnlyAgents
        } else if self.agents.is_some() || self.skills.is_some() {
            FilterMode::Include {
                agents: self.agents.clone().unwrap_or_default(),
                skills: self.skills.clone().unwrap_or_default(),
            }
        } else if self.exclude.is_some() {
            FilterMode::Exclude(self.exclude.clone().unwrap_or_default())
        } else {
            FilterMode::All
        }
    }

    /// Returns true if any filter field is set (not default).
    pub fn has_any_filter(&self) -> bool {
        self.agents.is_some()
            || self.skills.is_some()
            || self.exclude.is_some()
            || self.only_skills
            || self.only_agents
    }
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
    let reparsed: Config = toml::from_str(&content).map_err(|e| ConfigError::Invalid {
        message: format!("refusing to save config: serialized output failed to parse: {e}"),
    })?;
    validate_save_roundtrip(config, &reparsed)?;
    crate::fs::atomic_write(&path, content.as_bytes())
}

fn validate_save_roundtrip(original: &Config, reparsed: &Config) -> Result<(), MarsError> {
    if reparsed.dependencies.len() != original.dependencies.len() {
        return Err(ConfigError::Invalid {
            message: format!(
                "refusing to save config: dependency count changed during roundtrip ({} -> {})",
                original.dependencies.len(),
                reparsed.dependencies.len()
            ),
        }
        .into());
    }

    if reparsed.settings.managed_root != original.settings.managed_root {
        return Err(ConfigError::Invalid {
            message: format!(
                "refusing to save config: settings.managed_root changed during roundtrip ({:?} -> {:?})",
                original.settings.managed_root, reparsed.settings.managed_root
            ),
        }
        .into());
    }

    for (name, dep) in &original.dependencies {
        let Some(reparsed_dep) = reparsed.dependencies.get(name) else {
            return Err(ConfigError::Invalid {
                message: format!(
                    "refusing to save config: dependency `{name}` missing after roundtrip"
                ),
            }
            .into());
        };

        if reparsed_dep != dep {
            return Err(ConfigError::Invalid {
                message: format!(
                    "refusing to save config: dependency `{name}` changed during roundtrip"
                ),
            }
            .into());
        }
    }

    Ok(())
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
        let source = &effective.dependencies["base"];
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
        let source = &effective.dependencies["base"];
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
    fn roundtrip_full_config_shape_survives_save() {
        let dir = TempDir::new().unwrap();
        let original = r#"
[package]
name = "sample"
version = "0.1.0"
description = "sample package"

[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0"
agents = ["coder", "reviewer"]

[dependencies.local]
path = "../local-agents"
exclude = ["experimental"]

[settings]
managed_root = ".custom-agents"
links = [".claude", ".cursor"]
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let config = load(dir.path()).unwrap();
        save(dir.path(), &config).unwrap();
        let reloaded = load(dir.path()).unwrap();

        assert_eq!(
            reloaded.package.as_ref().map(|p| p.name.as_str()),
            Some("sample")
        );
        assert_eq!(reloaded.dependencies.len(), 2);
        assert_eq!(
            reloaded.dependencies["base"].url.as_deref(),
            Some("https://github.com/org/base.git")
        );
        assert_eq!(
            reloaded.dependencies["local"].path.as_deref(),
            Some(Path::new("../local-agents"))
        );
        assert_eq!(
            reloaded.settings.managed_root.as_deref(),
            Some(".custom-agents")
        );
        assert_eq!(reloaded.settings.links, vec![".claude", ".cursor"]);
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
        assert_eq!(effective.dependencies.len(), 1);
        let source = &effective.dependencies["base"];
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
        let source = &effective.dependencies["base"];
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
        assert!(matches!(
            effective.dependencies["base"].filter,
            FilterMode::All
        ));
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
        let source = &effective.dependencies["base"];
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

    #[test]
    fn save_preserves_dependencies_when_clearing_last_link() {
        let dir = TempDir::new().unwrap();
        let original = r#"
[package]
name = "sample"
version = "0.1.0"

[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0"
agents = ["coder"]

[settings]
managed_root = ".agents"
links = [".claude"]
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let mut config = load(dir.path()).unwrap();
        config.settings.links.retain(|link| link != ".claude");
        save(dir.path(), &config).unwrap();

        let reloaded = load(dir.path()).unwrap();
        assert_eq!(
            reloaded.package.as_ref().map(|p| p.name.as_str()),
            Some("sample")
        );
        assert_eq!(
            reloaded.dependencies["base"].url.as_deref(),
            Some("https://github.com/org/base.git")
        );
        assert_eq!(
            reloaded.dependencies["base"].version.as_deref(),
            Some("v1.0")
        );
        assert_eq!(
            reloaded.dependencies["base"].filter.agents.as_deref(),
            Some(&["coder".into()][..])
        );
        assert_eq!(reloaded.settings.managed_root.as_deref(), Some(".agents"));
        assert!(reloaded.settings.links.is_empty());
    }

    #[test]
    fn roundtrip_preserves_all_filter_fields() {
        let dir = TempDir::new().unwrap();
        let original = r#"
[dependencies.include]
url = "https://github.com/org/include.git"
agents = ["coder", "reviewer"]
skills = ["review", "plan"]

[dependencies.include.rename]
coder = "core-coder"

[dependencies.exclude]
url = "https://github.com/org/exclude.git"
exclude = ["experimental", "deprecated"]

[dependencies.only_skills]
url = "https://github.com/org/skills.git"
only_skills = true

[dependencies.only_agents]
url = "https://github.com/org/agents.git"
only_agents = true
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let config = load(dir.path()).unwrap();
        save(dir.path(), &config).unwrap();
        let reloaded = load(dir.path()).unwrap();

        let include = &reloaded.dependencies["include"].filter;
        assert_eq!(
            include.agents.as_deref(),
            Some(&["coder".into(), "reviewer".into()][..])
        );
        assert_eq!(
            include.skills.as_deref(),
            Some(&["review".into(), "plan".into()][..])
        );
        assert_eq!(
            include.rename.as_ref().and_then(|r| r.get("coder")),
            Some(&"core-coder".into())
        );

        let exclude = &reloaded.dependencies["exclude"].filter;
        assert_eq!(
            exclude.exclude.as_deref(),
            Some(&["experimental".into(), "deprecated".into()][..])
        );

        let only_skills = &reloaded.dependencies["only_skills"].filter;
        assert!(only_skills.only_skills);
        assert!(!only_skills.only_agents);

        let only_agents = &reloaded.dependencies["only_agents"].filter;
        assert!(only_agents.only_agents);
        assert!(!only_agents.only_skills);
    }

    #[test]
    fn roundtrip_multiple_dependencies_with_distinct_filter_combos() {
        let dir = TempDir::new().unwrap();
        let original = r#"
[dependencies.git-include]
url = "https://github.com/org/git-include.git"
agents = ["coder"]

[dependencies.path-exclude]
path = "../local-source"
exclude = ["draft"]

[dependencies.git-only-skills]
url = "https://github.com/org/git-skills.git"
only_skills = true

[dependencies.git-only-agents]
url = "https://github.com/org/git-agents.git"
only_agents = true
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let config = load(dir.path()).unwrap();
        save(dir.path(), &config).unwrap();
        let reloaded = load(dir.path()).unwrap();

        assert_eq!(reloaded.dependencies.len(), 4);
        assert_eq!(
            reloaded.dependencies["git-include"]
                .filter
                .agents
                .as_deref(),
            Some(&["coder".into()][..])
        );
        assert_eq!(
            reloaded.dependencies["path-exclude"].path.as_deref(),
            Some(Path::new("../local-source"))
        );
        assert_eq!(
            reloaded.dependencies["path-exclude"]
                .filter
                .exclude
                .as_deref(),
            Some(&["draft".into()][..])
        );
        assert!(reloaded.dependencies["git-only-skills"].filter.only_skills);
        assert!(reloaded.dependencies["git-only-agents"].filter.only_agents);
    }

    #[test]
    fn save_roundtrip_guard_rejects_dependency_count_loss() {
        let mut original = Config::default();
        original.dependencies.insert(
            "base".into(),
            DependencyEntry {
                url: Some("https://github.com/org/base.git".into()),
                path: None,
                version: Some("v1.0".into()),
                filter: FilterConfig::default(),
            },
        );

        let reparsed = Config::default();
        let err = validate_save_roundtrip(&original, &reparsed).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("dependency count changed"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn save_roundtrip_guard_rejects_managed_root_loss() {
        let original = Config {
            settings: Settings {
                managed_root: Some(".agents".into()),
                links: vec![],
            },
            ..Config::default()
        };
        let reparsed = Config::default();
        let err = validate_save_roundtrip(&original, &reparsed).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("settings.managed_root changed"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn parse_only_skills_filter() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
only_skills = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();
        let source = &effective.dependencies["base"];
        assert!(matches!(source.filter, FilterMode::OnlySkills));
    }

    #[test]
    fn parse_only_agents_filter() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
only_agents = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();
        let source = &effective.dependencies["base"];
        assert!(matches!(source.filter, FilterMode::OnlyAgents));
    }

    #[test]
    fn error_on_only_skills_and_only_agents() {
        let toml_str = r#"
[dependencies.bad]
url = "https://github.com/org/bad.git"
only_skills = true
only_agents = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("mutually exclusive"),
            "should mention mutually exclusive: {err}"
        );
    }

    #[test]
    fn error_on_only_skills_with_agents_list() {
        let toml_str = r#"
[dependencies.bad]
url = "https://github.com/org/bad.git"
only_skills = true
agents = ["coder"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot combine"),
            "should mention cannot combine: {err}"
        );
    }

    #[test]
    fn error_on_only_agents_with_skills_list() {
        let toml_str = r#"
[dependencies.bad]
url = "https://github.com/org/bad.git"
only_agents = true
skills = ["planning"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_only_skills_with_exclude() {
        let toml_str = r#"
[dependencies.bad]
url = "https://github.com/org/bad.git"
only_skills = true
exclude = ["deprecated"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
    }

    #[test]
    fn only_skills_false_not_serialized() {
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
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(
            !serialized.contains("only_skills"),
            "false booleans should not be serialized: {serialized}"
        );
        assert!(
            !serialized.contains("only_agents"),
            "false booleans should not be serialized: {serialized}"
        );
    }

    #[test]
    fn only_skills_true_roundtrips() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"
only_skills = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.dependencies["base"].filter.only_skills);
        assert!(!config.dependencies["base"].filter.only_agents);

        let serialized = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&serialized).unwrap();
        assert!(reloaded.dependencies["base"].filter.only_skills);
    }

    #[test]
    fn filter_config_has_any_filter() {
        assert!(!FilterConfig::default().has_any_filter());
        assert!(
            FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            }
            .has_any_filter()
        );
        assert!(
            FilterConfig {
                agents: Some(vec!["coder".into()]),
                ..FilterConfig::default()
            }
            .has_any_filter()
        );
    }

    #[test]
    fn filter_config_to_mode() {
        assert!(matches!(FilterConfig::default().to_mode(), FilterMode::All));
        assert!(matches!(
            FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            }
            .to_mode(),
            FilterMode::OnlySkills
        ));
        assert!(matches!(
            FilterConfig {
                only_agents: true,
                ..FilterConfig::default()
            }
            .to_mode(),
            FilterMode::OnlyAgents
        ));
        assert!(matches!(
            FilterConfig {
                agents: Some(vec!["coder".into()]),
                ..FilterConfig::default()
            }
            .to_mode(),
            FilterMode::Include { .. }
        ));
        assert!(matches!(
            FilterConfig {
                exclude: Some(vec!["old".into()]),
                ..FilterConfig::default()
            }
            .to_mode(),
            FilterMode::Exclude(_)
        ));
    }
}
