use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticCategory, DiagnosticLevel};
use crate::error::{ConfigError, MarsError};
use crate::types::{
    ItemName, RenameMap, SourceId, SourceName, SourceOrigin, SourceSubpath, SourceUrl,
};

/// Top-level mars.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageInfo>,
    #[serde(default)]
    pub dependencies: IndexMap<SourceName, InstallDep>,
    /// Local-only dependencies — installed when syncing this repo but NOT
    /// exported to consumers via manifest. Use for dev tooling, prompt
    /// authoring helpers, etc.
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        rename = "local-dependencies"
    )]
    pub local_dependencies: IndexMap<SourceName, InstallDep>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub models: IndexMap<String, crate::models::ModelAlias>,
}

/// Package metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

mod toml_path_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = path.to_string_lossy().replace('\\', "/");
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(PathBuf::from(s))
    }
}

mod toml_path_serde_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S>(path: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match path {
            Some(path) => {
                let s = path.to_string_lossy().replace('\\', "/");
                serializer.serialize_some(&s)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = Option::<String>::deserialize(deserializer)?;
        Ok(s.map(PathBuf::from))
    }
}

/// Consumer install intent — what goes in [dependencies] of a consumer mars.toml.
/// Has optional URL or path source plus filters for selecting items.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstallDep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<SourceUrl>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "toml_path_serde_opt"
    )]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<SourceSubpath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(flatten)]
    pub filter: FilterConfig,
}

/// Backwards-compatible alias during migration.
pub type DependencyEntry = InstallDep;

/// Package manifest dependency — what a package declares its consumers need.
/// Supports both URL (for remote consumers) and path (for local development).
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestDep {
    pub url: Option<SourceUrl>,
    pub path: Option<PathBuf>,
    pub subpath: Option<SourceSubpath>,
    pub version: Option<String>,
    pub filter: FilterConfig,
}

/// Source-manifest view extracted from mars.toml.
///
/// In source repositories, `mars.toml` may include `[package]` +
/// `[dependencies]` only, or coexist with consumer sections.
/// Dependencies are ManifestDep (URL or path, matching the source config).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub package: PackageInfo,
    pub dependencies: IndexMap<String, ManifestDep>,
    pub models: IndexMap<String, crate::models::ModelAlias>,
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

/// Display visibility filter for `mars models list`.
/// Consumer-only — lives under [settings], not [models].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelVisibility {
    /// Show only aliases matching these glob patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Hide aliases matching these glob patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

impl ModelVisibility {
    pub fn validate(&self) -> Result<(), MarsError> {
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_none() && self.exclude.is_none()
    }
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
    #[serde(with = "toml_path_serde")]
    pub path: PathBuf,
}

/// Global settings — extensible via additional fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Custom managed output directory (e.g. ".claude").
    ///
    /// When unset, mars no longer creates a generic `.agents` target by default;
    /// `.mars/` is the canonical compiled store and native emission is handled
    /// by target-specific compiler paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_root: Option<String>,
    /// Managed target directories materialized from .mars/ canonical store.
    /// When set, only listed targets are populated. When unset, `managed_root`
    /// is used for backwards compatibility; otherwise no target-sync targets
    /// are enabled by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "ModelVisibility::is_empty")]
    pub model_visibility: ModelVisibility,
    #[serde(default = "default_models_cache_ttl_hours")]
    pub models_cache_ttl_hours: u32,
    /// Minimum mars binary version required to use this project.
    /// Old binary + new package with this set → compatibility error.
    /// New binary + old package without this set → succeeds with defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_mars_version: Option<String>,
    /// Controls whether harness-bound agents are emitted to native harness dirs.
    ///
    /// `auto` (the default when unset) emits for standalone mars syncs and
    /// suppresses native agent artifacts when Meridian invokes mars with
    /// `MERIDIAN_MANAGED=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_emission: Option<AgentEmission>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentEmission {
    Auto,
    Always,
    Never,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            managed_root: None,
            targets: None,
            model_visibility: ModelVisibility::default(),
            models_cache_ttl_hours: default_models_cache_ttl_hours(),
            min_mars_version: None,
            agent_emission: None,
        }
    }
}

fn default_models_cache_ttl_hours() -> u32 {
    24
}

impl Settings {
    /// Returns the effective list of managed target directories.
    ///
    /// - If `targets` is explicitly set, returns exactly those targets.
    /// - If `targets` is unset, uses `managed_root` for backwards compatibility.
    /// - If neither is set, returns no target-sync targets; `.mars/` remains
    ///   the canonical compiled store.
    pub fn managed_targets(&self) -> Vec<String> {
        if let Some(targets) = &self.targets {
            return targets.clone();
        }
        self.managed_root.clone().into_iter().collect()
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub subpath: Option<SourceSubpath>,
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
///
/// Converts `InstallDep` entries to `ManifestDep`, preserving both URL and
/// path dependencies.
pub fn load_manifest(source_root: &Path) -> Result<(Option<Manifest>, Vec<Diagnostic>), MarsError> {
    let path = source_root.join(CONFIG_FILE);
    let diagnostics = Vec::new();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let parsed: Config =
                toml::from_str(&content).map_err(|e| crate::error::ConfigError::Invalid {
                    message: format!("failed to parse {}: {e}", path.display()),
                })?;
            let Some(package) = parsed.package else {
                return Ok((None, diagnostics));
            };
            // Convert InstallDep → ManifestDep, preserving both URL and path deps
            let deps: IndexMap<String, ManifestDep> = parsed
                .dependencies
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name.to_string(),
                        ManifestDep {
                            url: entry.url,
                            path: entry.path,
                            subpath: entry.subpath,
                            version: entry.version,
                            filter: entry.filter,
                        },
                    )
                })
                .collect();
            Ok((
                Some(Manifest {
                    package,
                    dependencies: deps,
                    models: parsed.models,
                }),
                diagnostics,
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, diagnostics)),
        Err(source) => Err(MarsError::Io {
            operation: "read manifest config".to_string(),
            path,
            source,
        }),
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
/// - Collects diagnostics if an override references a source name not in config
pub fn merge(config: Config, local: LocalConfig) -> Result<EffectiveConfig, MarsError> {
    let (effective, _diagnostics) = merge_with_root(config, local, Path::new("."))?;
    Ok(effective)
}

/// Same as `merge`, but uses an explicit root for path-based SourceId canonicalization.
pub fn merge_with_root(
    config: Config,
    local: LocalConfig,
    root: &Path,
) -> Result<(EffectiveConfig, Vec<Diagnostic>), MarsError> {
    config.settings.model_visibility.validate()?;
    let mut dependencies = IndexMap::new();
    let mut diagnostics = Vec::new();
    let local_source_name = SourceOrigin::LocalPackage.to_string();

    diagnostics.extend(deprecated_agents_target_diagnostics(&config.settings));

    // Process both regular and local dependencies into the same effective map.
    // Local deps are installed locally but not exported to consumers via manifest.
    let all_deps = config
        .dependencies
        .iter()
        .chain(config.local_dependencies.iter());

    for (name, entry) in all_deps {
        // Reject reserved name
        if name.as_ref() == local_source_name.as_str() {
            return Err(ConfigError::Invalid {
                message: "dependency name `_self` is reserved for local package items".into(),
            }
            .into());
        }

        // Reject duplicate names across sections
        if dependencies.contains_key(name) {
            return Err(ConfigError::Invalid {
                message: format!(
                    "dependency `{name}` appears in both [dependencies] and [local-dependencies]"
                ),
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
        let subpath = entry.subpath.clone();
        let id = source_id_for_spec(root, &spec, subpath.clone());

        dependencies.insert(
            name.clone(),
            EffectiveDependency {
                name: name.clone(),
                id,
                spec,
                subpath,
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
            diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warning,
                code: "override-missing-dep",
                message: format!(
                    "override `{override_name}` references a dependency not in mars.toml"
                ),
                context: None,
                category: None,
            });
        }
    }

    Ok((
        EffectiveConfig {
            dependencies,
            settings: config.settings,
        },
        diagnostics,
    ))
}

fn deprecated_agents_target_diagnostics(settings: &Settings) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    if settings.managed_root.as_deref() == Some(".agents") {
        diagnostics.push(deprecated_agents_target_diagnostic("settings.managed_root"));
    }

    if settings
        .targets
        .as_ref()
        .is_some_and(|targets| targets.iter().any(|target| target == ".agents"))
    {
        diagnostics.push(deprecated_agents_target_diagnostic("settings.targets"));
    }

    diagnostics
}

fn deprecated_agents_target_diagnostic(context: &str) -> Diagnostic {
    Diagnostic {
        level: DiagnosticLevel::Warning,
        code: "deprecated-agents-target",
        message: "`.agents` is a deprecated link target. Run `mars unlink .agents` to remove it. Skills are now emitted to native harness dirs automatically.".to_string(),
        context: Some(context.to_string()),
        category: Some(DiagnosticCategory::Compatibility),
    }
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

fn source_id_for_spec(root: &Path, spec: &SourceSpec, subpath: Option<SourceSubpath>) -> SourceId {
    match spec {
        SourceSpec::Git(git) => SourceId::git_with_subpath(git.url.clone(), subpath.clone()),
        SourceSpec::Path(path) => match SourceId::path_with_subpath(root, path, subpath.clone()) {
            Ok(id) => id,
            Err(_) => {
                let canonical = if path.is_absolute() {
                    path.clone()
                } else {
                    root.join(path)
                };
                SourceId::Path { canonical, subpath }
            }
        },
    }
}

fn migrate_legacy_source_urls(config: &mut Config) {
    for dep in config
        .dependencies
        .values_mut()
        .chain(config.local_dependencies.values_mut())
    {
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

    if reparsed.local_dependencies.len() != original.local_dependencies.len() {
        return Err(ConfigError::Invalid {
            message: format!(
                "refusing to save config: local-dependencies count changed during roundtrip ({} -> {})",
                original.local_dependencies.len(),
                reparsed.local_dependencies.len()
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
    if reparsed.settings.model_visibility != original.settings.model_visibility {
        return Err(ConfigError::Invalid {
            message: format!(
                "refusing to save config: settings.model_visibility changed during roundtrip ({:?} -> {:?})",
                original.settings.model_visibility, reparsed.settings.model_visibility
            ),
        }
        .into());
    }
    if reparsed.settings.agent_emission != original.settings.agent_emission {
        return Err(ConfigError::Invalid {
            message: format!(
                "refusing to save config: settings.agent_emission changed during roundtrip ({:?} -> {:?})",
                original.settings.agent_emission, reparsed.settings.agent_emission
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

    for (name, dep) in &original.local_dependencies {
        let Some(reparsed_dep) = reparsed.local_dependencies.get(name) else {
            return Err(ConfigError::Invalid {
                message: format!(
                    "refusing to save config: local-dependency `{name}` missing after roundtrip"
                ),
            }
            .into());
        };

        if reparsed_dep != dep {
            return Err(ConfigError::Invalid {
                message: format!(
                    "refusing to save config: local-dependency `{name}` changed during roundtrip"
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
targets = [".claude", ".cursor"]
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
        assert_eq!(
            reloaded.settings.targets,
            Some(vec![".claude".to_string(), ".cursor".to_string()])
        );
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

        let (manifest, diagnostics) = load_manifest(dir.path()).unwrap();
        assert!(diagnostics.is_empty());
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
skills = ["frontend-design"]
"#,
        )
        .unwrap();

        let (manifest, diagnostics) = load_manifest(dir.path()).unwrap();
        assert!(diagnostics.is_empty());
        let manifest = manifest.unwrap();
        assert_eq!(manifest.package.name, "pkg");
        assert_eq!(manifest.package.version, "1.2.3");
        assert!(manifest.dependencies.contains_key("base"));
        assert_eq!(
            manifest.dependencies["base"].filter.skills.as_deref(),
            Some(&[ItemName::from("frontend-design")][..])
        );
    }

    #[test]
    fn load_manifest_io_error_includes_operation_and_path() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("mars.toml");
        std::fs::create_dir(&config_path).unwrap();

        let err = load_manifest(dir.path()).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("read manifest config"),
            "error should include operation context: {msg}"
        );
        assert!(
            msg.contains("mars.toml"),
            "error should include config path: {msg}"
        );
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
                        subpath: None,
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
                        subpath: None,
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
    fn merge_override_retains_subpath_coordinate() {
        let temp = TempDir::new().unwrap();
        // Canonicalize temp root once to avoid Windows 8.3 short-name mismatches
        let temp_root = dunce::canonicalize(temp.path()).unwrap();
        let override_path = temp_root.join("local-base");
        std::fs::create_dir_all(&override_path).unwrap();
        let canonical_override = dunce::canonicalize(&override_path).unwrap();

        let config = Config {
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "base".into(),
                    DependencyEntry {
                        url: Some("https://github.com/org/base.git".into()),
                        path: None,
                        subpath: Some(SourceSubpath::new("plugins/foo").unwrap()),
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
                        path: canonical_override.clone(),
                    },
                );
                m
            },
        };

        let (effective, _) = merge_with_root(config, local, &temp_root).unwrap();
        let source = &effective.dependencies["base"];
        assert!(source.is_overridden);
        assert_eq!(
            source.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
        assert!(matches!(&source.spec, SourceSpec::Path(p) if p == &canonical_override));
        assert!(matches!(
            &source.id,
            SourceId::Path {
                canonical,
                subpath: Some(sp)
            } if canonical == &canonical_override && sp.as_str() == "plugins/foo"
        ));
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
                        subpath: None,
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
                        subpath: None,
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
                targets: None,
                ..Settings::default()
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
    fn save_preserves_dependencies_when_clearing_last_target() {
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
targets = [".claude"]
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let mut config = load(dir.path()).unwrap();
        if let Some(targets) = config.settings.targets.as_mut() {
            targets.retain(|target| target != ".claude");
            if targets.is_empty() {
                config.settings.targets = None;
            }
        }
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
        assert!(reloaded.settings.targets.is_none());
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
                subpath: None,
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
                targets: None,
                ..Settings::default()
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
                        subpath: None,
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

    // === managed_targets tests ===

    #[test]
    fn managed_targets_defaults_to_no_target_sync_targets() {
        let settings = Settings::default();
        assert!(settings.managed_targets().is_empty());
    }

    #[test]
    fn managed_targets_uses_explicit_targets() {
        let settings = Settings {
            targets: Some(vec![".claude".to_string()]),
            ..Settings::default()
        };
        assert_eq!(settings.managed_targets(), vec![".claude"]);
    }

    #[test]
    fn managed_targets_uses_managed_root_as_primary() {
        let settings = Settings {
            managed_root: Some(".claude".to_string()),
            ..Settings::default()
        };
        assert_eq!(settings.managed_targets(), vec![".claude"]);
    }

    #[test]
    fn managed_targets_explicit_overrides_links_and_managed_root() {
        let settings = Settings {
            managed_root: Some(".cursor".to_string()),
            targets: Some(vec![".codex".to_string()]),
            ..Settings::default()
        };
        // targets takes precedence over managed_root
        assert_eq!(settings.managed_targets(), vec![".codex"]);
    }

    #[test]
    fn merge_warns_when_managed_root_is_agents() {
        let config = Config {
            settings: Settings {
                managed_root: Some(".agents".into()),
                ..Settings::default()
            },
            ..Config::default()
        };

        let (_, diagnostics) =
            merge_with_root(config, LocalConfig::default(), Path::new(".")).unwrap();

        assert!(diagnostics.iter().any(|diag| {
            diag.code == "deprecated-agents-target"
                && diag.context.as_deref() == Some("settings.managed_root")
        }));
    }

    #[test]
    fn merge_warns_when_targets_include_agents() {
        let config = Config {
            settings: Settings {
                targets: Some(vec![".agents".into(), ".claude".into()]),
                ..Settings::default()
            },
            ..Config::default()
        };

        let (_, diagnostics) =
            merge_with_root(config, LocalConfig::default(), Path::new(".")).unwrap();

        assert!(diagnostics.iter().any(|diag| {
            diag.code == "deprecated-agents-target"
                && diag.context.as_deref() == Some("settings.targets")
        }));
    }

    #[test]
    fn settings_models_cache_ttl_defaults_to_24_when_omitted() {
        let config: Config = toml::from_str(
            r#"
[dependencies.base]
url = "https://github.com/org/base.git"
"#,
        )
        .unwrap();
        assert_eq!(config.settings.models_cache_ttl_hours, 24);
    }

    #[test]
    fn settings_models_cache_ttl_defaults_to_24_when_settings_present_without_ttl() {
        let config: Config = toml::from_str(
            r#"
[settings]
managed_root = ".agents"
"#,
        )
        .unwrap();
        assert_eq!(config.settings.models_cache_ttl_hours, 24);
    }

    #[test]
    fn settings_models_cache_ttl_parses_zero() {
        let config: Config = toml::from_str(
            r#"
[settings]
models_cache_ttl_hours = 0
"#,
        )
        .unwrap();
        assert_eq!(config.settings.models_cache_ttl_hours, 0);
    }

    #[test]
    fn settings_models_cache_ttl_parses_custom_value() {
        let config: Config = toml::from_str(
            r#"
[settings]
models_cache_ttl_hours = 48
"#,
        )
        .unwrap();
        assert_eq!(config.settings.models_cache_ttl_hours, 48);
    }

    #[test]
    fn settings_models_cache_ttl_roundtrip_preserves_value() {
        let original = Config {
            settings: Settings {
                models_cache_ttl_hours: 48,
                ..Settings::default()
            },
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&original).unwrap();
        let roundtripped: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            roundtripped.settings.models_cache_ttl_hours,
            original.settings.models_cache_ttl_hours
        );
    }

    #[test]
    fn settings_agent_emission_parses_auto() {
        let config: Config = toml::from_str(
            r#"
[settings]
agent_emission = "auto"
"#,
        )
        .unwrap();
        assert_eq!(config.settings.agent_emission, Some(AgentEmission::Auto));
    }

    #[test]
    fn settings_agent_emission_parses_always_and_never() {
        let always: Config = toml::from_str(
            r#"
[settings]
agent_emission = "always"
"#,
        )
        .unwrap();
        assert_eq!(always.settings.agent_emission, Some(AgentEmission::Always));

        let never: Config = toml::from_str(
            r#"
[settings]
agent_emission = "never"
"#,
        )
        .unwrap();
        assert_eq!(never.settings.agent_emission, Some(AgentEmission::Never));
    }

    #[test]
    fn settings_agent_emission_defaults_to_auto_when_omitted() {
        let config: Config = toml::from_str(
            r#"
[settings]
models_cache_ttl_hours = 48
"#,
        )
        .unwrap();
        assert!(config.settings.agent_emission.is_none());
    }

    #[test]
    fn settings_agent_emission_roundtrip_preserves_value() {
        let original = Config {
            settings: Settings {
                agent_emission: Some(AgentEmission::Always),
                ..Settings::default()
            },
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&original).unwrap();
        let roundtripped: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            roundtripped.settings.agent_emission,
            original.settings.agent_emission
        );
    }

    #[test]
    fn model_visibility_validate_allows_include_and_exclude() {
        let visibility = ModelVisibility {
            include: Some(vec!["opus*".into()]),
            exclude: Some(vec!["test*".into()]),
        };
        visibility.validate().unwrap();
    }

    #[test]
    fn model_visibility_validate_allows_include_only_exclude_only_and_empty() {
        ModelVisibility {
            include: Some(vec!["opus*".into()]),
            exclude: None,
        }
        .validate()
        .unwrap();
        ModelVisibility {
            include: None,
            exclude: Some(vec!["test*".into()]),
        }
        .validate()
        .unwrap();
        ModelVisibility::default().validate().unwrap();
    }

    #[test]
    fn model_visibility_is_empty_reports_state() {
        assert!(ModelVisibility::default().is_empty());
        assert!(
            !ModelVisibility {
                include: Some(vec!["opus*".into()]),
                exclude: None,
            }
            .is_empty()
        );
        assert!(
            !ModelVisibility {
                include: None,
                exclude: Some(vec!["test*".into()]),
            }
            .is_empty()
        );
    }

    #[test]
    fn load_accepts_model_visibility_with_include_and_exclude() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[settings.model_visibility]
include = ["opus*"]
exclude = ["test*"]
"#,
        )
        .unwrap();

        let config = load(dir.path()).unwrap();
        assert_eq!(
            config.settings.model_visibility.include,
            Some(vec!["opus*".into()])
        );
        assert_eq!(
            config.settings.model_visibility.exclude,
            Some(vec!["test*".into()])
        );
    }

    #[test]
    fn load_accepts_model_visibility_include_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[settings.model_visibility]
include = ["opus*", "gpt-*"]
"#,
        )
        .unwrap();

        let config = load(dir.path()).unwrap();
        assert_eq!(
            config.settings.model_visibility.include,
            Some(vec!["opus*".into(), "gpt-*".into()])
        );
        assert!(config.settings.model_visibility.exclude.is_none());
    }

    #[test]
    fn load_accepts_model_visibility_exclude_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[settings.model_visibility]
exclude = ["test-*", "deprecated-*"]
"#,
        )
        .unwrap();

        let config = load(dir.path()).unwrap();
        assert_eq!(
            config.settings.model_visibility.exclude,
            Some(vec!["test-*".into(), "deprecated-*".into()])
        );
        assert!(config.settings.model_visibility.include.is_none());
    }

    // === local-dependencies tests ===

    #[test]
    fn parse_local_dependencies() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"

[local-dependencies.prompter]
url = "https://github.com/org/prompter.git"
skills = ["prompt-helper"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.dependencies.len(), 1);
        assert_eq!(config.local_dependencies.len(), 1);
        assert!(config.local_dependencies.contains_key("prompter"));
        assert_eq!(
            config.local_dependencies["prompter"].url.as_deref(),
            Some("https://github.com/org/prompter.git")
        );
    }

    #[test]
    fn local_dependencies_merged_into_effective_config() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"

[local-dependencies.prompter]
url = "https://github.com/org/prompter.git"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let effective = merge(config, local).unwrap();

        // Both deps should be in effective config
        assert_eq!(effective.dependencies.len(), 2);
        assert!(effective.dependencies.contains_key("base"));
        assert!(effective.dependencies.contains_key("prompter"));
    }

    #[test]
    fn local_dependencies_not_exported_to_manifest() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            r#"
[package]
name = "my-package"
version = "1.0.0"

[dependencies.base]
url = "https://github.com/org/base.git"

[local-dependencies.prompter]
url = "https://github.com/org/prompter.git"
"#,
        )
        .unwrap();

        let (manifest, diagnostics) = load_manifest(dir.path()).unwrap();
        assert!(diagnostics.is_empty());
        let manifest = manifest.unwrap();

        // Only base should be in manifest, not prompter
        assert_eq!(manifest.dependencies.len(), 1);
        assert!(manifest.dependencies.contains_key("base"));
        assert!(!manifest.dependencies.contains_key("prompter"));
    }

    #[test]
    fn error_on_duplicate_name_across_sections() {
        let toml_str = r#"
[dependencies.base]
url = "https://github.com/org/base.git"

[local-dependencies.base]
url = "https://github.com/org/base-local.git"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let local = LocalConfig::default();
        let result = merge(config, local);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("base") && err.contains("both"),
            "should reject duplicate name: {err}"
        );
    }

    #[test]
    fn local_dependencies_roundtrip() {
        let dir = TempDir::new().unwrap();
        let original = r#"
[dependencies.base]
url = "https://github.com/org/base.git"

[local-dependencies.prompter]
url = "https://github.com/org/prompter.git"
skills = ["prompt-helper"]
"#;
        std::fs::write(dir.path().join("mars.toml"), original).unwrap();

        let config = load(dir.path()).unwrap();
        save(dir.path(), &config).unwrap();
        let reloaded = load(dir.path()).unwrap();

        assert_eq!(reloaded.dependencies.len(), 1);
        assert_eq!(reloaded.local_dependencies.len(), 1);
        assert!(reloaded.local_dependencies.contains_key("prompter"));
        assert_eq!(
            reloaded.local_dependencies["prompter"]
                .filter
                .skills
                .as_deref(),
            Some(&["prompt-helper".into()][..])
        );
    }

    #[test]
    fn path_with_backslashes_serializes_as_forward_slashes() {
        let mut deps = IndexMap::new();
        deps.insert(
            SourceName::from("test-src"),
            InstallDep {
                url: None,
                path: Some(PathBuf::from("C:\\Users\\dev\\src")),
                subpath: None,
                version: None,
                filter: FilterConfig::default(),
            },
        );
        let config = Config {
            dependencies: deps,
            ..Config::default()
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(
            !toml_str.contains('\\'),
            "TOML output must not contain backslashes: {toml_str}"
        );
        assert!(
            toml_str.contains("C:/Users/dev/src"),
            "expected forward-slash path in TOML: {toml_str}"
        );
        let reparsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            reparsed.dependencies["test-src"].path.as_ref().unwrap(),
            &PathBuf::from("C:/Users/dev/src"),
        );
    }

    #[test]
    fn override_path_serializes_forward_slashes() {
        let mut overrides = IndexMap::new();
        overrides.insert(
            SourceName::from("my-dep"),
            OverrideEntry {
                path: PathBuf::from("C:\\Users\\dev\\local-pkg"),
            },
        );
        let local = LocalConfig { overrides };
        let toml_str = toml::to_string_pretty(&local).unwrap();
        assert!(
            !toml_str.contains('\\'),
            "local config TOML must not contain backslashes: {toml_str}"
        );
        assert!(
            toml_str.contains("C:/Users/dev/local-pkg"),
            "expected forward-slash override path: {toml_str}"
        );
    }
}
