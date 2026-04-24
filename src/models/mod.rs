//! Model catalog — aliases with direct model pinning and optional discovery filters,
//! dependency-tree config merge, and models cache lifecycle.
//!
//! Model aliases map short names (opus, sonnet, codex) to concrete model IDs.
//! Two modes:
//! - **Pinned**: explicit model ID, with optional `match`/`exclude` discovery filters.
//! - **AutoResolve**: pattern-based resolution against a cached model catalog.
//!
//! Merge precedence: consumer > deps (declaration order).

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::types::MarsContext;

pub mod harness;

mod tracing {
    macro_rules! debug {
        ($($arg:tt)*) => {
            if cfg!(debug_assertions) {
                eprintln!($($arg)*);
            }
        };
    }

    pub(super) use debug;
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A model alias — either pinned to a specific model ID or auto-resolved
/// against the models cache at resolution time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelAlias {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autocompact: Option<u8>,
    #[serde(flatten)]
    pub spec: ModelSpec,
}

/// How a model alias resolves to a concrete model ID.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelSpec {
    /// Explicit model ID — no resolution needed.
    Pinned {
        model: String,
        provider: Option<String>,
    },
    /// Explicit model ID for resolution, plus discovery filters for list/all views.
    PinnedWithMatch {
        model: String,
        provider: Option<String>,
        match_patterns: Vec<String>,
        exclude_patterns: Vec<String>,
    },
    /// Pattern-based resolution against models cache.
    AutoResolve {
        provider: String,
        match_patterns: Vec<String>,
        exclude_patterns: Vec<String>,
    },
}

/// How the harness was determined.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSource {
    Explicit,
    AutoDetected,
    Unavailable,
}

/// Fully resolved model alias — everything a consumer needs to launch.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedAlias {
    pub name: String,
    pub model_id: String,
    pub provider: String,
    pub harness: Option<String>,
    pub harness_source: HarnessSource,
    pub harness_candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autocompact: Option<u8>,
}

// Custom Serialize for ModelSpec to flatten into parent
impl Serialize for ModelSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ModelSpec::Pinned { model, provider } => {
                let mut count = 1;
                if provider.is_some() {
                    count += 1;
                }
                let mut map = serializer.serialize_map(Some(count))?;
                map.serialize_entry("model", model)?;
                if let Some(provider) = provider {
                    map.serialize_entry("provider", provider)?;
                }
                map.end()
            }
            ModelSpec::PinnedWithMatch {
                model,
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                let mut count = 2; // model + match
                if provider.is_some() {
                    count += 1;
                }
                if !exclude_patterns.is_empty() {
                    count += 1;
                }
                let mut map = serializer.serialize_map(Some(count))?;
                map.serialize_entry("model", model)?;
                map.serialize_entry("match", match_patterns)?;
                if let Some(provider) = provider {
                    map.serialize_entry("provider", provider)?;
                }
                if !exclude_patterns.is_empty() {
                    map.serialize_entry("exclude", exclude_patterns)?;
                }
                map.end()
            }
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                let mut count = 2; // provider + match
                if !exclude_patterns.is_empty() {
                    count += 1;
                }
                let mut map = serializer.serialize_map(Some(count))?;
                map.serialize_entry("provider", provider)?;
                map.serialize_entry("match", match_patterns)?;
                if !exclude_patterns.is_empty() {
                    map.serialize_entry("exclude", exclude_patterns)?;
                }
                map.end()
            }
        }
    }
}

/// Raw deserialization helper — distinguished by field presence.
#[derive(Debug, Deserialize)]
struct RawModelAlias {
    harness: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_effort: Option<String>,
    #[serde(default)]
    autocompact: Option<toml::Value>,
    // Pinned mode
    #[serde(default)]
    model: Option<String>,
    // AutoResolve mode
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "match")]
    match_patterns: Option<Vec<String>>,
    #[serde(default)]
    exclude: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for ModelAlias {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawModelAlias::deserialize(deserializer)?;
        let default_effort = raw.default_effort.filter(|value| !value.trim().is_empty());
        if let Some(ref effort) = default_effort {
            const VALID_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "auto"];
            if !VALID_EFFORTS.contains(&effort.as_str()) {
                return Err(serde::de::Error::custom(format!(
                    "invalid default_effort '{effort}'; accepted values: {}",
                    VALID_EFFORTS.join(", ")
                )));
            }
        }
        let autocompact: Option<u8> = match raw.autocompact {
            Some(toml::Value::Integer(value)) if (1..=100).contains(&value) => Some(value as u8),
            Some(toml::Value::Integer(value)) => {
                return Err(serde::de::Error::custom(format!(
                    "autocompact {value} is out of range 1-100"
                )));
            }
            Some(other) => {
                return Err(serde::de::Error::custom(format!(
                    "autocompact must be an integer 1-100, got {other:?}"
                )));
            }
            None => None,
        };

        let has_match = raw.match_patterns.is_some();

        let spec = if let Some(model) = raw.model {
            if !has_match && raw.exclude.is_some() {
                return Err(serde::de::Error::custom(
                    "model alias with 'exclude' must also include 'match'",
                ));
            }
            if let Some(match_patterns) = raw.match_patterns {
                ModelSpec::PinnedWithMatch {
                    model,
                    provider: raw.provider,
                    match_patterns,
                    exclude_patterns: raw.exclude.unwrap_or_default(),
                }
            } else {
                ModelSpec::Pinned {
                    model,
                    provider: raw.provider,
                }
            }
        } else if let Some(match_patterns) = raw.match_patterns {
            let provider = raw.provider.ok_or_else(|| {
                serde::de::Error::custom(
                    "auto-resolve model alias requires 'provider' when 'match' is specified",
                )
            })?;
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns: raw.exclude.unwrap_or_default(),
            }
        } else {
            return Err(serde::de::Error::custom(
                "model alias must have either 'model' (pinned) or 'match' (auto-resolve)",
            ));
        };

        Ok(ModelAlias {
            harness: raw.harness,
            description: raw.description,
            default_effort,
            autocompact,
            spec,
        })
    }
}

// ---------------------------------------------------------------------------
// Models cache
// ---------------------------------------------------------------------------

/// Cached model catalog from external API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsCache {
    pub models: Vec<CachedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
}

/// A single model entry in the cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModel {
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
}

const CACHE_FILE: &str = "models-cache.json";
const FETCH_FAIL_MARKER_FILE: &str = ".models-cache.last-fail";
const DEFAULT_MODELS_CACHE_TTL_HOURS: u32 = 24;
pub(crate) const FETCH_FAIL_COOLDOWN_SECS: u64 = 300;
const FETCH_FAIL_COOLDOWN_REASON: &str = "recent fetch attempt failed; backing off (cooldown)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    Auto,
    Force,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    AlreadyFresh,
    Refreshed { models_count: usize },
    StaleFallback { reason: String },
    Offline,
}

pub fn now_unix_secs_value() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn now_unix_secs() -> String {
    now_unix_secs_value().to_string()
}

pub fn is_mars_offline() -> bool {
    match std::env::var("MARS_OFFLINE") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        ),
        Err(_) => false,
    }
}

pub fn resolve_refresh_mode(no_refresh_flag: bool) -> RefreshMode {
    if no_refresh_flag {
        RefreshMode::Offline
    } else {
        RefreshMode::Auto
    }
}

pub fn load_models_cache_ttl(ctx: &MarsContext) -> u32 {
    crate::config::load(&ctx.project_root)
        .map(|config| config.settings.models_cache_ttl_hours)
        .unwrap_or(DEFAULT_MODELS_CACHE_TTL_HOURS)
}

fn read_cache_tolerant(mars_dir: &Path) -> ModelsCache {
    match read_cache(mars_dir) {
        Ok(cache) => cache,
        Err(err) => {
            tracing::debug!("models cache read failed, treating as empty: {err}");
            ModelsCache {
                models: Vec::new(),
                fetched_at: None,
            }
        }
    }
}

fn is_fresh(cache: &ModelsCache, ttl_hours: u32) -> bool {
    if ttl_hours == 0 {
        return false;
    }
    if cache.models.is_empty() {
        return false;
    }

    let Some(fetched_str) = &cache.fetched_at else {
        return false;
    };
    let Ok(fetched) = fetched_str.parse::<u64>() else {
        return false;
    };

    let now = now_unix_secs_value();
    if fetched > now {
        return false;
    }

    (now - fetched) < (ttl_hours as u64) * 3600
}

fn is_usable(cache: &ModelsCache) -> bool {
    !cache.models.is_empty()
}

fn read_fetch_fail_marker(mars_dir: &Path) -> Option<u64> {
    let marker = mars_dir.join(FETCH_FAIL_MARKER_FILE);
    let raw = std::fs::read_to_string(marker).ok()?;
    raw.trim().parse::<u64>().ok()
}

fn write_fetch_fail_marker(mars_dir: &Path, timestamp: u64) {
    let marker = mars_dir.join(FETCH_FAIL_MARKER_FILE);
    if let Err(err) = crate::fs::atomic_write(&marker, timestamp.to_string().as_bytes()) {
        tracing::debug!("failed to write models fetch failure marker: {err}");
    }
}

fn clear_fetch_fail_marker(mars_dir: &Path) {
    let marker = mars_dir.join(FETCH_FAIL_MARKER_FILE);
    if let Err(err) = std::fs::remove_file(marker)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!("failed to clear models fetch failure marker: {err}");
    }
}

pub fn ensure_fresh(
    mars_dir: &Path,
    ttl_hours: u32,
    mode: RefreshMode,
) -> Result<(ModelsCache, RefreshOutcome), MarsError> {
    ensure_fresh_with_fetcher(mars_dir, ttl_hours, mode, fetch_models)
}

fn ensure_fresh_with_fetcher<F>(
    mars_dir: &Path,
    ttl_hours: u32,
    mode: RefreshMode,
    fetcher: F,
) -> Result<(ModelsCache, RefreshOutcome), MarsError>
where
    F: FnOnce() -> Result<Vec<CachedModel>, MarsError>,
{
    std::fs::create_dir_all(mars_dir)?;

    // D1: apply MARS_OFFLINE coercion exactly once here.
    let effective_mode = match mode {
        RefreshMode::Auto if is_mars_offline() => RefreshMode::Offline,
        m => m,
    };

    let prior = read_cache_tolerant(mars_dir);

    if effective_mode == RefreshMode::Auto && is_fresh(&prior, ttl_hours) {
        return Ok((prior, RefreshOutcome::AlreadyFresh));
    }

    if effective_mode == RefreshMode::Offline {
        if is_usable(&prior) {
            return Ok((prior, RefreshOutcome::Offline));
        }
        return Err(MarsError::ModelCacheUnavailable {
            reason: offline_unavailable_reason(mode),
        });
    }

    let lock_path = mars_dir.join(".models-cache.lock");
    let _guard = crate::fs::FileLock::acquire(&lock_path)?;

    let under_lock = read_cache_tolerant(mars_dir);
    if effective_mode == RefreshMode::Auto && is_fresh(&under_lock, ttl_hours) {
        return Ok((under_lock, RefreshOutcome::AlreadyFresh));
    }

    if mode != RefreshMode::Force && is_usable(&under_lock) {
        let now = now_unix_secs_value();
        if let Some(last_fail) = read_fetch_fail_marker(mars_dir)
            && now.saturating_sub(last_fail) < FETCH_FAIL_COOLDOWN_SECS
        {
            return Ok((
                under_lock,
                RefreshOutcome::StaleFallback {
                    reason: FETCH_FAIL_COOLDOWN_REASON.to_string(),
                },
            ));
        }
    }

    match fetcher() {
        Ok(models) if !models.is_empty() => {
            let models_count = models.len();
            let cache = ModelsCache {
                models,
                fetched_at: Some(now_unix_secs()),
            };
            write_cache(mars_dir, &cache)?;
            clear_fetch_fail_marker(mars_dir);
            Ok((cache, RefreshOutcome::Refreshed { models_count }))
        }
        Ok(_) => fallback_to_stale_or_error(
            mars_dir,
            under_lock,
            "API returned empty catalog".to_string(),
            "API returned an empty catalog and no prior cache exists".to_string(),
            true,
        ),
        Err(err) => fallback_to_stale_or_error(
            mars_dir,
            under_lock,
            format!("fetch failed: {err}"),
            format!("automatic refresh failed: {err}"),
            true,
        ),
    }
}

fn fallback_to_stale_or_error(
    mars_dir: &Path,
    under_lock: ModelsCache,
    stale_reason: String,
    unavailable_reason: String,
    mark_fetch_failure: bool,
) -> Result<(ModelsCache, RefreshOutcome), MarsError> {
    if is_usable(&under_lock) {
        if mark_fetch_failure {
            write_fetch_fail_marker(mars_dir, now_unix_secs_value());
        }
        Ok((
            under_lock,
            RefreshOutcome::StaleFallback {
                reason: stale_reason,
            },
        ))
    } else {
        Err(MarsError::ModelCacheUnavailable {
            reason: unavailable_reason,
        })
    }
}

fn offline_unavailable_reason(requested_mode: RefreshMode) -> String {
    match requested_mode {
        RefreshMode::Offline => {
            "--no-refresh-models was passed and no cached catalog is available".to_string()
        }
        RefreshMode::Auto => "MARS_OFFLINE is set and no cached catalog is available".to_string(),
        RefreshMode::Force => "MARS_OFFLINE is set and no cached catalog is available".to_string(),
    }
}

/// Read models cache from `.mars/models-cache.json`.
pub fn read_cache(mars_dir: &Path) -> Result<ModelsCache, MarsError> {
    let path = mars_dir.join(CACHE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let cache: ModelsCache =
                serde_json::from_str(&content).map_err(|e| crate::error::ConfigError::Invalid {
                    message: format!("failed to parse models cache: {e}"),
                })?;
            Ok(cache)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        }),
        Err(source) => Err(MarsError::Io {
            operation: "read models cache".to_string(),
            path,
            source,
        }),
    }
}

/// Write models cache to `.mars/models-cache.json` (atomic via tmp+rename).
pub fn write_cache(mars_dir: &Path, cache: &ModelsCache) -> Result<(), MarsError> {
    std::fs::create_dir_all(mars_dir)?;
    let path = mars_dir.join(CACHE_FILE);
    let tmp_path = mars_dir.join(".models-cache.json.tmp");
    let content =
        serde_json::to_string_pretty(cache).map_err(|e| crate::error::ConfigError::Invalid {
            message: format!("failed to serialize models cache: {e}"),
        })?;
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Fetch models from the models.dev API.
///
/// Returns a list of cached model entries. On network failure, returns an error
/// (callers should fall back to existing cache or explicit pinned IDs).
pub fn fetch_models() -> Result<Vec<CachedModel>, MarsError> {
    let url = models_api_url();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(15)))
        .build()
        .into();

    let response = agent.get(&url).call().map_err(|e| match e {
        ureq::Error::StatusCode(status) => MarsError::Http {
            url: url.clone(),
            status,
            message: format!("request failed with HTTP status {status}"),
        },
        _ => MarsError::Http {
            url: url.clone(),
            status: 0,
            message: format!("failed to fetch models catalog: {e}"),
        },
    })?;
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| MarsError::Http {
            url: url.clone(),
            status: 0,
            message: format!("failed to read response body: {e}"),
        })?;
    let raw: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| crate::error::ConfigError::Invalid {
            message: format!("failed to parse models API response: {e}"),
        })?;

    parse_models_dev_catalog(&raw)
}

fn models_api_url() -> String {
    std::env::var("MARS_MODELS_API_URL").unwrap_or_else(|_| "https://models.dev/api.json".into())
}

fn parse_models_dev_catalog(raw: &serde_json::Value) -> Result<Vec<CachedModel>, MarsError> {
    let providers = raw
        .as_object()
        .ok_or_else(|| crate::error::ConfigError::Invalid {
            message: "models API response must be an object keyed by provider".to_string(),
        })?;

    let mut models = Vec::new();

    for (provider_key, provider_obj) in providers {
        if !is_major_provider(provider_key) {
            continue;
        }

        let Some(provider_models) = provider_obj.get("models").and_then(|m| m.as_object()) else {
            continue;
        };

        for model_obj in provider_models.values() {
            let Some(model_id) = model_obj.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let release_date = model_obj
                .get("release_date")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let description = model_obj
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let context_window = model_obj
                .get("limit")
                .and_then(|v| v.get("context"))
                .and_then(|v| v.as_u64());
            let max_output = model_obj
                .get("limit")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_u64());

            models.push(CachedModel {
                id: model_id.to_string(),
                provider: normalize_provider(provider_key),
                release_date,
                description,
                context_window,
                max_output,
            });
        }
    }

    Ok(models)
}

fn is_major_provider(provider_key: &str) -> bool {
    matches!(
        provider_key,
        "anthropic"
            | "openai"
            | "google"
            | "meta-llama"
            | "meta"
            | "mistralai"
            | "mistral"
            | "deepseek"
            | "cohere"
    )
}

/// Normalize models.dev provider keys to canonical names.
fn normalize_provider(slug: &str) -> String {
    match slug {
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "google" => "Google".to_string(),
        "meta-llama" | "meta" => "Meta".to_string(),
        "mistralai" | "mistral" => "Mistral".to_string(),
        "deepseek" => "DeepSeek".to_string(),
        "cohere" => "Cohere".to_string(),
        _ => slug.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Auto-resolve algorithm
// ---------------------------------------------------------------------------

/// Resolve an auto-resolve spec against the models cache.
///
/// Algorithm:
/// 1. Filter by provider (case-insensitive)
/// 2. All match patterns must hit (AND)
/// 3. No exclude patterns may hit (OR)
/// 4. Skip entries ending with `-latest` (synthetic aliases)
/// 5. Sort by newest release_date, then shortest ID, then lexical ID
/// 6. Return all candidates
pub fn auto_resolve_all<'a>(
    provider: &str,
    match_patterns: &[String],
    exclude_patterns: &[String],
    cache: &'a ModelsCache,
) -> Vec<&'a CachedModel> {
    let mut candidates: Vec<&CachedModel> = cache
        .models
        .iter()
        .filter(|m| {
            // Provider match (case-insensitive)
            m.provider.eq_ignore_ascii_case(provider)
        })
        .filter(|m| {
            // Skip -latest suffix (synthetic aliases)
            !m.id.ends_with("-latest")
        })
        .filter(|m| {
            // All match patterns must hit (AND)
            match_patterns.iter().all(|p| glob_match(p, &m.id))
        })
        .filter(|m| {
            // No exclude patterns may hit (OR)
            !exclude_patterns.iter().any(|p| glob_match(p, &m.id))
        })
        .collect();

    // Sort: newest release_date first, then shortest ID, then lexical ID.
    candidates.sort_by(|a, b| {
        let date_cmp = b
            .release_date
            .as_deref()
            .unwrap_or("")
            .cmp(a.release_date.as_deref().unwrap_or(""));
        date_cmp
            .then_with(|| a.id.len().cmp(&b.id.len()))
            .then_with(|| a.id.cmp(&b.id))
    });

    candidates
}

/// Resolve an auto-resolve spec against the models cache.
///
/// Algorithm:
/// 1. Filter by provider (case-insensitive)
/// 2. All match patterns must hit (AND)
/// 3. No exclude patterns may hit (OR)
/// 4. Skip entries ending with `-latest` (synthetic aliases)
/// 5. Sort by newest release_date, then shortest ID, then lexical ID
/// 6. Pick first
pub fn auto_resolve(
    provider: &str,
    match_patterns: &[String],
    exclude_patterns: &[String],
    cache: &ModelsCache,
) -> Option<String> {
    auto_resolve_all(provider, match_patterns, exclude_patterns, cache)
        .first()
        .map(|model| model.id.clone())
}

/// Resolve an input like `opus-4-6` by matching it against alias filter candidates.
///
/// Algorithm:
/// 1. Build a glob pattern `*{input}*` from the user input
/// 2. For each auto-resolve alias, run its filters against the cache
/// 3. From those candidates, keep models matching the glob
/// 4. Collect union across aliases, deduplicated by model ID
/// 5. Sort by newest release_date, then shortest ID
/// 6. Return the best candidate
pub fn resolve_with_alias_prefix(
    input: &str,
    aliases: &IndexMap<String, ModelAlias>,
    cache: &ModelsCache,
) -> Option<ResolvedAlias> {
    let pattern = if input.contains('*') {
        input.to_string()
    } else {
        format!("*{}*", input)
    };
    let base_alias = alias_prefix_base(input, aliases);
    let mut deduped: IndexMap<String, CachedModel> = IndexMap::new();

    if let Some(alias) = base_alias
        && let Some((model, provider)) = match &alias.spec {
            ModelSpec::Pinned { model, provider } => Some((model, provider)),
            ModelSpec::PinnedWithMatch {
                model, provider, ..
            } => Some((model, provider)),
            ModelSpec::AutoResolve { .. } => None,
        }
    {
        let provider_filter = provider
            .as_deref()
            .or_else(|| infer_provider_from_model_id(model));
        for candidate in &cache.models {
            if !glob_match(&pattern, &candidate.id) {
                continue;
            }
            if let Some(provider_filter) = provider_filter
                && !candidate.provider.eq_ignore_ascii_case(provider_filter)
            {
                continue;
            }
            deduped
                .entry(candidate.id.clone())
                .or_insert_with(|| candidate.clone());
        }
    }

    for (_alias_name, alias) in aliases {
        match &alias.spec {
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                for candidate in auto_resolve_all(provider, match_patterns, exclude_patterns, cache)
                {
                    if glob_match(&pattern, &candidate.id) {
                        deduped
                            .entry(candidate.id.clone())
                            .or_insert_with(|| candidate.clone());
                    }
                }
            }
            ModelSpec::PinnedWithMatch {
                model,
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                let Some(provider) = provider
                    .as_deref()
                    .or_else(|| infer_provider_from_model_id(model))
                else {
                    continue;
                };
                for candidate in auto_resolve_all(provider, match_patterns, exclude_patterns, cache)
                {
                    if glob_match(&pattern, &candidate.id) {
                        deduped
                            .entry(candidate.id.clone())
                            .or_insert_with(|| candidate.clone());
                    }
                }
            }
            ModelSpec::Pinned { .. } => {}
        }
    }

    let mut candidates: Vec<CachedModel> = deduped.into_values().collect();
    candidates.sort_by(|a, b| {
        let date_cmp = b
            .release_date
            .as_deref()
            .unwrap_or("")
            .cmp(a.release_date.as_deref().unwrap_or(""));
        date_cmp
            .then_with(|| a.id.len().cmp(&b.id.len()))
            .then_with(|| a.id.cmp(&b.id))
    });

    let winner = candidates.into_iter().next()?;
    let provider = winner.provider.to_ascii_lowercase();
    let (default_effort, autocompact) = match base_alias {
        Some(ModelAlias {
            default_effort,
            autocompact,
            spec: ModelSpec::Pinned { .. } | ModelSpec::PinnedWithMatch { .. },
            ..
        }) => (default_effort.clone(), *autocompact),
        _ => (None, None),
    };
    let installed = harness::detect_installed_harnesses();
    let harness = harness::resolve_harness_for_provider(&provider, &installed);
    let harness_source = if harness.is_some() {
        HarnessSource::AutoDetected
    } else {
        HarnessSource::Unavailable
    };

    Some(ResolvedAlias {
        name: input.to_string(),
        model_id: winner.id,
        provider: provider.clone(),
        harness,
        harness_source,
        harness_candidates: harness::harness_candidates_for_provider(&provider),
        description: winner.description,
        default_effort,
        autocompact,
    })
}

fn alias_prefix_base<'a>(
    input: &str,
    aliases: &'a IndexMap<String, ModelAlias>,
) -> Option<&'a ModelAlias> {
    aliases
        .iter()
        .filter(|(name, _)| {
            !name.is_empty()
                && input.len() > name.len()
                && input.starts_with(name.as_str())
                && input.as_bytes().get(name.len()) == Some(&b'-')
        })
        .max_by_key(|(name, _)| name.len())
        .map(|(_, alias)| alias)
}

/// Simple glob matching: `*` matches any sequence of characters.
/// Everything else is literal. Case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Split pattern on '*' and match segments in order
    let segments: Vec<&str> = pattern.split('*').collect();

    if segments.len() == 1 {
        // No wildcards — exact match
        return pattern == text;
    }

    let mut pos = 0;

    // First segment must be a prefix
    if let Some(first) = segments.first()
        && !first.is_empty()
    {
        if !text.starts_with(first) {
            return false;
        }
        pos = first.len();
    }

    // Last segment must be a suffix
    if let Some(last) = segments.last()
        && !last.is_empty()
        && !text[pos..].ends_with(last)
    {
        return false;
    }

    // Middle segments must appear in order
    let end = if let Some(last) = segments.last() {
        if !last.is_empty() {
            text.len() - last.len()
        } else {
            text.len()
        }
    } else {
        text.len()
    };

    for segment in &segments[1..segments.len().saturating_sub(1)] {
        if segment.is_empty() {
            continue;
        }
        if let Some(idx) = text[pos..end].find(segment) {
            pos += idx + segment.len();
        } else {
            return false;
        }
    }

    pos <= end
}

// ---------------------------------------------------------------------------
// Builtin aliases — bare convenience mappings, no descriptions
// ---------------------------------------------------------------------------

/// Minimal builtin aliases so common model names work out of the box.
/// No descriptions — packages layer those on top.
/// Precedence: consumer > deps > builtins.
pub fn builtin_aliases() -> IndexMap<String, ModelAlias> {
    let mut m = IndexMap::new();
    let add = |m: &mut IndexMap<String, ModelAlias>,
               name: &str,
               provider: &str,
               match_patterns: &[&str],
               exclude: &[&str]| {
        m.insert(
            name.to_string(),
            ModelAlias {
                harness: None,
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::AutoResolve {
                    provider: provider.to_string(),
                    match_patterns: match_patterns.iter().map(|s| s.to_string()).collect(),
                    exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
                },
            },
        );
    };
    add(&mut m, "opus", "anthropic", &["*opus*"], &[]);
    add(&mut m, "sonnet", "anthropic", &["*sonnet*"], &[]);
    add(&mut m, "haiku", "anthropic", &["*haiku*"], &[]);
    add(
        &mut m,
        "codex",
        "openai",
        &["*codex*"],
        &["*-mini", "*-spark", "*-max"],
    );
    add(
        &mut m,
        "gpt",
        "openai",
        &["gpt-5*"],
        &["*codex*", "*-mini", "*-nano", "*-chat", "*-turbo"],
    );
    add(
        &mut m,
        "gemini",
        "google",
        &["gemini*", "*pro*"],
        &["*-customtools"],
    );
    m
}

// ---------------------------------------------------------------------------
// Dependency-tree merge
// ---------------------------------------------------------------------------

/// Info about a resolved dependency's model config.
pub struct ResolvedDepModels {
    pub source_name: String,
    pub models: IndexMap<String, ModelAlias>,
}

/// Merge model aliases from dependency tree.
///
/// Precedence: consumer > deps (declaration order) > builtins.
/// When two deps define the same alias, first in declaration order wins
/// with a diagnostic warning.
pub fn merge_model_config(
    consumer: &IndexMap<String, ModelAlias>,
    deps: &[ResolvedDepModels],
    diag: &mut DiagnosticCollector,
    cache: Option<&ModelsCache>,
) -> IndexMap<String, ModelAlias> {
    #[derive(Clone)]
    struct DepWinner {
        source_name: String,
        alias: ModelAlias,
    }

    let mut merged = IndexMap::new();
    let builtins = builtin_aliases();

    // Layer 0 (lowest): builtins
    for (name, alias) in &builtins {
        merged.insert(name.clone(), alias.clone());
    }

    // Track which dep won each alias (vs builtin)
    let mut dep_provided: std::collections::HashMap<String, DepWinner> =
        std::collections::HashMap::new();

    // Layer 1: dependencies (override builtins silently, first dep wins on conflicts)
    for dep in deps {
        for (name, alias) in &dep.models {
            if consumer.contains_key(name) {
                // Consumer will override — skip dep's version silently
                continue;
            }
            if let Some(winner) = dep_provided.get(name) {
                // Two deps define same alias — first dep wins, warn
                let message = if let Some(cache) = cache {
                    let (winner_formatted, winner_model_id) =
                        format_alias_resolution_for_diag(&winner.alias, &winner.source_name, cache);
                    let (loser_formatted, loser_model_id) =
                        format_alias_resolution_for_diag(alias, &dep.source_name, cache);
                    if winner_model_id.is_some() && winner_model_id == loser_model_id {
                        format!(
                            "model alias `{name}` defined by both `{}` and `{}` — using {} (declared first)\n  both resolve to {}\n  → add [models.{name}] to your mars.toml to resolve explicitly",
                            winner.source_name,
                            dep.source_name,
                            winner.source_name,
                            winner_model_id.unwrap_or_default(),
                        )
                    } else {
                        format!(
                            "model alias `{name}` defined by both `{}` and `{}` — using {} (declared first)\n  {winner_formatted}, {loser_formatted}\n  → add [models.{name}] to your mars.toml to resolve explicitly",
                            winner.source_name, dep.source_name, winner.source_name,
                        )
                    }
                } else {
                    format!(
                        "model alias `{name}` defined by both `{}` and `{}` — using {} (declared first)\n  → add [models.{name}] to your mars.toml to resolve explicitly",
                        winner.source_name, dep.source_name, winner.source_name,
                    )
                };
                diag.warn_with_context("model-alias-conflict", message, dep.source_name.clone());
            } else {
                // Override builtin or insert new
                merged.insert(name.clone(), alias.clone());
                dep_provided.insert(
                    name.clone(),
                    DepWinner {
                        source_name: dep.source_name.clone(),
                        alias: alias.clone(),
                    },
                );
            }
        }
    }

    // Layer 2 (highest): consumer config
    for (name, alias) in consumer {
        merged.insert(name.clone(), alias.clone());
    }

    merged
}

/// Resolve all aliases to concrete model IDs + harnesses.
///
/// Harness detection is encapsulated — callers don't pass installed harnesses.
pub fn resolve_all(
    aliases: &IndexMap<String, ModelAlias>,
    cache: &ModelsCache,
    diag: &mut DiagnosticCollector,
) -> IndexMap<String, ResolvedAlias> {
    let _ = diag;
    let installed = harness::detect_installed_harnesses();
    let mut resolved = IndexMap::new();

    for (name, alias) in aliases {
        let Some((model_id, provider)) = resolve_model_and_provider(alias, cache) else {
            continue; // unresolvable — omit
        };

        let candidates = harness::harness_candidates_for_provider(&provider);
        let (h, source) = resolve_harness(alias, &provider, &installed);

        resolved.insert(
            name.clone(),
            ResolvedAlias {
                name: name.clone(),
                model_id,
                provider,
                harness: h,
                harness_source: source,
                harness_candidates: candidates,
                description: alias.description.clone(),
                default_effort: alias.default_effort.clone(),
                autocompact: alias.autocompact,
            },
        );
    }

    resolved
}

/// Resolve a single alias and emit diagnostics only for that alias.
pub fn resolve_one(
    name: &str,
    aliases: &IndexMap<String, ModelAlias>,
    cache: &ModelsCache,
    diag: &mut DiagnosticCollector,
) -> Option<ResolvedAlias> {
    let alias = aliases.get(name)?;
    let installed = harness::detect_installed_harnesses();
    let (model_id, provider) = resolve_model_and_provider(alias, cache)?;
    let candidates = harness::harness_candidates_for_provider(&provider);
    let (harness, harness_source) = resolve_harness(alias, &provider, &installed);
    let _ = diag;
    Some(ResolvedAlias {
        name: name.to_string(),
        model_id,
        provider,
        harness,
        harness_source,
        harness_candidates: candidates,
        description: alias.description.clone(),
        default_effort: alias.default_effort.clone(),
        autocompact: alias.autocompact,
    })
}

/// Filter resolved aliases by visibility config.
/// - `include` patterns: keep only aliases where at least one pattern matches
/// - `exclude` patterns: remove aliases where any pattern matches
/// - No config (both None): return all aliases unchanged
pub fn filter_by_visibility(
    mut aliases: IndexMap<String, ResolvedAlias>,
    visibility: &crate::config::ModelVisibility,
) -> IndexMap<String, ResolvedAlias> {
    if let Some(includes) = &visibility.include {
        aliases.retain(|name, _| includes.iter().any(|p| glob_match(p, name)));
    } else if let Some(excludes) = &visibility.exclude {
        aliases.retain(|name, _| !excludes.iter().any(|p| glob_match(p, name)));
    }
    aliases
}

fn resolve_model_and_provider(alias: &ModelAlias, cache: &ModelsCache) -> Option<(String, String)> {
    match &alias.spec {
        ModelSpec::Pinned {
            model, provider, ..
        } => {
            let p = provider
                .clone()
                .or_else(|| infer_provider_from_model_id(model).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            Some((model.clone(), p))
        }
        ModelSpec::PinnedWithMatch {
            model, provider, ..
        } => {
            let p = provider
                .clone()
                .or_else(|| infer_provider_from_model_id(model).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string());
            Some((model.clone(), p))
        }
        ModelSpec::AutoResolve {
            provider,
            match_patterns,
            exclude_patterns,
        } => {
            let model_id = auto_resolve(provider, match_patterns, exclude_patterns, cache)?;
            Some((model_id, provider.clone()))
        }
    }
}

fn format_alias_resolution_for_diag(
    alias: &ModelAlias,
    source_name: &str,
    cache: &ModelsCache,
) -> (String, Option<String>) {
    match &alias.spec {
        ModelSpec::Pinned { model, .. } => (
            format!("{source_name} → {model} (pinned)"),
            Some(model.clone()),
        ),
        ModelSpec::PinnedWithMatch { model, .. } => (
            format!("{source_name} → {model} (pinned+match)"),
            Some(model.clone()),
        ),
        ModelSpec::AutoResolve {
            provider,
            match_patterns,
            exclude_patterns,
        } => {
            let resolved = auto_resolve(provider, match_patterns, exclude_patterns, cache);
            match resolved {
                Some(model_id) => (format!("{source_name} → {model_id}"), Some(model_id)),
                None => (format!("{source_name} → <unresolvable>"), None),
            }
        }
    }
}

fn resolve_harness(
    alias: &ModelAlias,
    provider: &str,
    installed: &HashSet<String>,
) -> (Option<String>, HarnessSource) {
    if let Some(h) = &alias.harness {
        if installed.contains(h) {
            (Some(h.clone()), HarnessSource::Explicit)
        } else {
            (Some(h.clone()), HarnessSource::Unavailable)
        }
    } else {
        match harness::resolve_harness_for_provider(provider, installed) {
            Some(h) => (Some(h), HarnessSource::AutoDetected),
            None => (None, HarnessSource::Unavailable),
        }
    }
}

/// Best-effort provider inference from model ID prefixes.
/// Returns None for unrecognized patterns.
pub fn infer_provider_from_model_id(model_id: &str) -> Option<&'static str> {
    let id = model_id.to_lowercase();
    if id.starts_with("claude-") {
        return Some("anthropic");
    }
    if id.starts_with("gpt-")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4")
        || id.starts_with("codex-")
    {
        return Some("openai");
    }
    if id.starts_with("gemini") {
        return Some("google");
    }
    if id.starts_with("llama") {
        return Some("meta");
    }
    if id.starts_with("mistral") || id.starts_with("codestral") {
        return Some("mistral");
    }
    if id.starts_with("deepseek") {
        return Some("deepseek");
    }
    if id.starts_with("command") {
        return Some("cohere");
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use tempfile::tempdir;

    use serial_test::serial;

    #[test]
    fn parse_models_dev_catalog_maps_fields_and_filters_providers() {
        let raw = serde_json::json!({
            "anthropic": {
                "models": {
                    "claude-opus-4-6": {
                        "id": "claude-opus-4-6",
                        "name": "Claude Opus 4.6",
                        "release_date": "2026-02-05",
                        "limit": {
                            "context": 1000000,
                            "output": 128000
                        }
                    }
                }
            },
            "openai": {
                "models": {
                    "gpt-5": {
                        "id": "gpt-5",
                        "name": "GPT-5"
                    }
                }
            },
            "random-host": {
                "models": {
                    "foo": {
                        "id": "foo"
                    }
                }
            }
        });

        let models = parse_models_dev_catalog(&raw).unwrap();
        assert_eq!(models.len(), 2);

        let opus = models
            .iter()
            .find(|m| m.id == "claude-opus-4-6")
            .expect("missing claude-opus-4-6");
        assert_eq!(opus.provider, "Anthropic");
        assert_eq!(opus.release_date.as_deref(), Some("2026-02-05"));
        assert_eq!(opus.description.as_deref(), Some("Claude Opus 4.6"));
        assert_eq!(opus.context_window, Some(1_000_000));
        assert_eq!(opus.max_output, Some(128_000));

        let gpt = models
            .iter()
            .find(|m| m.id == "gpt-5")
            .expect("missing gpt-5");
        assert_eq!(gpt.provider, "OpenAI");
        assert_eq!(gpt.release_date, None);
        assert_eq!(gpt.description.as_deref(), Some("GPT-5"));
        assert_eq!(gpt.context_window, None);
        assert_eq!(gpt.max_output, None);
    }

    #[test]
    fn parse_models_dev_catalog_requires_object_root() {
        let raw = serde_json::json!(["not", "an", "object"]);
        let err = parse_models_dev_catalog(&raw).unwrap_err();
        assert!(err.to_string().contains("keyed by provider"));
    }

    // -- glob_match tests --

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("claude-opus-4", "claude-opus-4"));
        assert!(!glob_match("claude-opus-4", "claude-opus-5"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(glob_match("claude-opus-*", "claude-opus-4"));
        assert!(glob_match("claude-opus-*", "claude-opus-4-20250514"));
        assert!(!glob_match("claude-opus-*", "claude-sonnet-4"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(glob_match("*-opus-4", "claude-opus-4"));
        assert!(!glob_match("*-opus-4", "claude-opus-5"));
    }

    #[test]
    fn glob_star_middle() {
        assert!(glob_match("claude-*-4", "claude-opus-4"));
        assert!(glob_match("claude-*-4", "claude-sonnet-4"));
        assert!(!glob_match("claude-*-4", "claude-opus-5"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("*claude*opus*", "claude-opus-4"));
        assert!(glob_match("*claude*opus*", "my-claude-opus-4-special"));
        assert!(!glob_match("*claude*opus*", "claude-sonnet-4"));
    }

    #[test]
    fn glob_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_empty_pattern() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "something"));
    }

    // -- auto_resolve tests --

    fn make_cache(models: Vec<(&str, &str, Option<&str>)>) -> ModelsCache {
        ModelsCache {
            models: models
                .into_iter()
                .map(|(id, provider, date)| CachedModel {
                    id: id.to_string(),
                    provider: provider.to_string(),
                    release_date: date.map(String::from),
                    description: None,
                    context_window: None,
                    max_output: None,
                })
                .collect(),
            fetched_at: Some("2025-01-01T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn auto_resolve_basic() {
        let cache = make_cache(vec![
            ("claude-opus-4", "Anthropic", Some("2025-03-01")),
            ("claude-opus-4-20250514", "Anthropic", Some("2025-05-14")),
            ("claude-sonnet-4", "Anthropic", Some("2025-03-01")),
        ]);

        let result = auto_resolve("Anthropic", &["claude-opus-*".to_string()], &[], &cache);
        // Newest date wins
        assert_eq!(result, Some("claude-opus-4-20250514".to_string()));
    }

    #[test]
    fn auto_resolve_exclude() {
        let cache = make_cache(vec![
            ("gpt-5", "OpenAI", Some("2025-06-01")),
            ("gpt-4o-mini", "OpenAI", Some("2024-07-01")),
            ("gpt-3.5-turbo", "OpenAI", Some("2023-03-01")),
        ]);

        let result = auto_resolve(
            "OpenAI",
            &["gpt-*".to_string()],
            &["gpt-3*".to_string(), "gpt-4o*".to_string()],
            &cache,
        );
        assert_eq!(result, Some("gpt-5".to_string()));
    }

    #[test]
    fn auto_resolve_skip_latest() {
        let cache = make_cache(vec![
            ("claude-opus-latest", "Anthropic", Some("9999-01-01")),
            ("claude-opus-4", "Anthropic", Some("2025-03-01")),
        ]);

        let result = auto_resolve("Anthropic", &["claude-opus-*".to_string()], &[], &cache);
        // Should skip -latest even though it has a newer date
        assert_eq!(result, Some("claude-opus-4".to_string()));
    }

    #[test]
    fn auto_resolve_empty_cache() {
        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let result = auto_resolve("Anthropic", &["claude-opus-*".to_string()], &[], &cache);
        assert_eq!(result, None);
    }

    #[test]
    fn auto_resolve_no_match() {
        let cache = make_cache(vec![("claude-opus-4", "Anthropic", Some("2025-03-01"))]);

        let result = auto_resolve("OpenAI", &["gpt-*".to_string()], &[], &cache);
        assert_eq!(result, None);
    }

    #[test]
    fn auto_resolve_provider_case_insensitive() {
        let cache = make_cache(vec![("claude-opus-4", "Anthropic", Some("2025-03-01"))]);

        let result = auto_resolve("anthropic", &["claude-opus-*".to_string()], &[], &cache);
        assert_eq!(result, Some("claude-opus-4".to_string()));
    }

    #[test]
    fn auto_resolve_shortest_id_tiebreaker() {
        let cache = make_cache(vec![
            ("claude-opus-4", "Anthropic", Some("2025-03-01")),
            ("claude-opus-4x", "Anthropic", Some("2025-03-01")),
        ]);

        let result = auto_resolve("Anthropic", &["claude-opus-*".to_string()], &[], &cache);
        // Same date — shorter ID wins
        assert_eq!(result, Some("claude-opus-4".to_string()));
    }

    #[test]
    fn auto_resolve_lexical_id_tiebreaker_when_date_and_length_equal() {
        let cache = make_cache(vec![
            ("claude-opus-4-b", "Anthropic", Some("2025-03-01")),
            ("claude-opus-4-a", "Anthropic", Some("2025-03-01")),
        ]);

        let result = auto_resolve("Anthropic", &["claude-opus-4-*".to_string()], &[], &cache);
        // Same date + same length — lexical ID wins for deterministic ordering.
        assert_eq!(result, Some("claude-opus-4-a".to_string()));
    }

    #[test]
    fn auto_resolve_all_returns_all_candidates() {
        let cache = make_cache(vec![
            ("claude-opus-4-5", "Anthropic", Some("2025-12-01")),
            ("claude-opus-latest", "Anthropic", Some("9999-01-01")),
            ("claude-opus-4-6-long", "Anthropic", Some("2026-02-05")),
            ("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
            ("claude-opus-3", "Anthropic", Some("2024-02-05")),
        ]);

        let result = auto_resolve_all(
            "Anthropic",
            &["claude-opus-*".to_string()],
            &["*opus-3".to_string()],
            &cache,
        );
        let ids: Vec<&str> = result.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude-opus-4-6", "claude-opus-4-6-long", "claude-opus-4-5"]
        );
    }

    // -- merge_model_config tests --

    fn pinned_alias(harness: Option<&str>, model: &str) -> ModelAlias {
        ModelAlias {
            harness: harness.map(|h| h.to_string()),
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: model.to_string(),
                provider: None,
            },
        }
    }

    fn auto_alias(
        provider: &str,
        match_patterns: &[&str],
        exclude_patterns: &[&str],
    ) -> ModelAlias {
        ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::AutoResolve {
                provider: provider.to_string(),
                match_patterns: match_patterns.iter().map(|s| s.to_string()).collect(),
                exclude_patterns: exclude_patterns.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    fn pinned_match_alias(
        model: &str,
        provider: &str,
        match_patterns: &[&str],
        exclude_patterns: &[&str],
    ) -> ModelAlias {
        ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::PinnedWithMatch {
                model: model.to_string(),
                provider: Some(provider.to_string()),
                match_patterns: match_patterns.iter().map(|s| s.to_string()).collect(),
                exclude_patterns: exclude_patterns.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn resolve_with_alias_prefix_basic() {
        let aliases = builtin_aliases();
        let cache = make_cache(vec![("claude-opus-4-6", "Anthropic", Some("2026-02-05"))]);

        let resolved = resolve_with_alias_prefix("opus-4-6", &aliases, &cache).unwrap();
        assert_eq!(resolved.name, "opus-4-6");
        assert_eq!(resolved.model_id, "claude-opus-4-6");
        assert_eq!(resolved.provider, "anthropic");
        assert_eq!(
            resolved.harness_candidates,
            vec!["claude", "opencode", "gemini"]
        );

        let installed = harness::detect_installed_harnesses();
        let expected_harness = harness::resolve_harness_for_provider("anthropic", &installed);
        let expected_source = if expected_harness.is_some() {
            HarnessSource::AutoDetected
        } else {
            HarnessSource::Unavailable
        };
        assert_eq!(resolved.harness, expected_harness);
        assert_eq!(resolved.harness_source, expected_source);
    }

    #[test]
    fn resolve_with_alias_prefix_no_candidates() {
        let aliases = builtin_aliases();
        let cache = make_cache(vec![("claude-opus-4-6", "Anthropic", Some("2026-02-05"))]);

        let resolved = resolve_with_alias_prefix("opus-9-9", &aliases, &cache);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_with_alias_prefix_picks_newest() {
        let aliases = builtin_aliases();
        let cache = make_cache(vec![
            ("claude-opus-4-6-20250101", "Anthropic", Some("2025-01-01")),
            ("claude-opus-4-6-20260101", "Anthropic", Some("2026-01-01")),
        ]);

        let resolved = resolve_with_alias_prefix("opus-4-6", &aliases, &cache).unwrap();
        assert_eq!(resolved.model_id, "claude-opus-4-6-20260101");
    }

    #[test]
    fn resolve_with_alias_prefix_lexical_id_tiebreaker_when_date_and_length_equal() {
        let aliases = builtin_aliases();
        let cache = make_cache(vec![
            ("claude-opus-4-b", "Anthropic", Some("2026-02-05")),
            ("claude-opus-4-a", "Anthropic", Some("2026-02-05")),
        ]);

        let resolved = resolve_with_alias_prefix("opus-4-", &aliases, &cache).unwrap();
        assert_eq!(resolved.model_id, "claude-opus-4-a");
    }

    #[test]
    fn resolve_with_alias_prefix_pinned_base_inherits_defaults() {
        let mut aliases = IndexMap::new();
        let mut alias = pinned_alias(Some("claude"), "claude-opus-4-6");
        alias.default_effort = Some("high".to_string());
        alias.autocompact = Some(42);
        aliases.insert("opus".to_string(), alias);
        let cache = make_cache(vec![("claude-opus-4-7", "Anthropic", Some("2026-04-16"))]);

        let resolved = resolve_with_alias_prefix("opus-4-7", &aliases, &cache).unwrap();
        assert_eq!(resolved.model_id, "claude-opus-4-7");
        assert_eq!(resolved.default_effort.as_deref(), Some("high"));
        assert_eq!(resolved.autocompact, Some(42));
    }

    #[test]
    fn resolve_with_alias_prefix_auto_base_does_not_inherit_defaults() {
        let mut aliases = IndexMap::new();
        let mut alias = auto_alias("anthropic", &["claude-opus-*"], &[]);
        alias.default_effort = Some("high".to_string());
        alias.autocompact = Some(42);
        aliases.insert("opus".to_string(), alias);
        let cache = make_cache(vec![("claude-opus-4-7", "Anthropic", Some("2026-04-16"))]);

        let resolved = resolve_with_alias_prefix("opus-4-7", &aliases, &cache).unwrap();
        assert_eq!(resolved.model_id, "claude-opus-4-7");
        assert_eq!(resolved.default_effort, None);
        assert_eq!(resolved.autocompact, None);
    }

    #[test]
    fn resolve_with_alias_prefix_exact_name_matches() {
        // When the input equals an alias name, this function still finds matches
        // via glob *opus*. The caller (run_resolve) handles exact alias lookup
        // before calling this function, so this path is only reached for
        // non-alias inputs in practice.
        let aliases = builtin_aliases();
        let cache = make_cache(vec![("claude-opus-4-6", "Anthropic", Some("2026-02-05"))]);

        let resolved = resolve_with_alias_prefix("opus", &aliases, &cache);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().model_id, "claude-opus-4-6");
    }

    #[test]
    fn resolve_with_alias_prefix_multiple_aliases_union() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "g".to_string(),
            auto_alias("openai", &["gpt-2026-08*"], &[]),
        );
        aliases.insert(
            "gpt".to_string(),
            auto_alias("openai", &["gpt-2026-03*"], &[]),
        );
        let cache = make_cache(vec![
            ("gpt-2026-03-01", "OpenAI", Some("2026-03-01")),
            ("gpt-2026-08-07", "OpenAI", Some("2026-08-07")),
        ]);

        let resolved = resolve_with_alias_prefix("gpt-2026", &aliases, &cache).unwrap();
        assert_eq!(resolved.model_id, "gpt-2026-08-07");
    }

    #[test]
    fn merge_empty_returns_builtins() {
        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[], &mut diag, None);
        // Empty consumer + no deps = builtins only
        assert!(merged.contains_key("opus"));
        assert!(merged.contains_key("sonnet"));
        assert!(merged.contains_key("codex"));
    }

    #[test]
    fn merge_consumer_overrides_dependency_alias() {
        let mut consumer = IndexMap::new();
        consumer.insert(
            "opus".to_string(),
            pinned_alias(Some("custom"), "my-opus-model"),
        );

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&consumer, &[], &mut diag, None);
        assert_eq!(
            merged.get("opus").unwrap().spec,
            ModelSpec::Pinned {
                model: "my-opus-model".to_string(),
                provider: None
            }
        );
    }

    #[test]
    fn merge_dep_overrides_builtin() {
        let dep = ResolvedDepModels {
            source_name: "my-pkg".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("opus".to_string(), pinned_alias(Some("custom"), "pkg-opus"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[dep], &mut diag, None);
        // Dep overrides builtin
        assert_eq!(
            merged.get("opus").unwrap().spec,
            ModelSpec::Pinned {
                model: "pkg-opus".to_string(),
                provider: None
            }
        );
    }

    #[test]
    fn merge_consumer_beats_dep() {
        let mut consumer = IndexMap::new();
        consumer.insert("opus".to_string(), pinned_alias(Some("c"), "consumer-opus"));

        let dep = ResolvedDepModels {
            source_name: "pkg".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("opus".to_string(), pinned_alias(Some("d"), "dep-opus"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&consumer, &[dep], &mut diag, None);
        assert_eq!(
            merged.get("opus").unwrap().spec,
            ModelSpec::Pinned {
                model: "consumer-opus".to_string(),
                provider: None
            }
        );
    }

    #[test]
    fn merge_dep_conflict_warns_with_winner_and_resolution_hint() {
        let dep1 = ResolvedDepModels {
            source_name: "pkg-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("a"), "model-a"));
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "pkg-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("b"), "model-b"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag, None);
        // First dep wins
        assert_eq!(
            merged.get("custom").unwrap().spec,
            ModelSpec::Pinned {
                model: "model-a".to_string(),
                provider: None
            }
        );
        // Should have warned
        let warnings = diag.drain();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "model-alias-conflict");
        assert_eq!(
            warnings[0].message,
            "model alias `custom` defined by both `pkg-a` and `pkg-b` — using pkg-a (declared first)\n  → add [models.custom] to your mars.toml to resolve explicitly"
        );
    }

    #[test]
    fn merge_dep_conflict_with_cache_shows_resolution_diff() {
        let cache = make_cache(vec![
            ("claude-opus-4-7", "Anthropic", Some("2026-04-16")),
            ("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
        ]);
        let dep1 = ResolvedDepModels {
            source_name: "dep-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert(
                    "opus".to_string(),
                    pinned_match_alias("claude-opus-4-6", "Anthropic", &["claude-opus-*"], &[]),
                );
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "dep-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert(
                    "opus".to_string(),
                    pinned_match_alias("claude-opus-4-7", "Anthropic", &["claude-opus-*"], &[]),
                );
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let _merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag, Some(&cache));
        let warnings = diag.drain();
        assert_eq!(warnings.len(), 1);
        let message = &warnings[0].message;
        assert!(message.contains("dep-a → claude-opus-4-6 (pinned+match)"));
        assert!(message.contains("dep-b → claude-opus-4-7 (pinned+match)"));
    }

    #[test]
    fn merge_dep_conflict_with_cache_same_resolution() {
        let cache = make_cache(vec![
            ("claude-opus-4-7", "Anthropic", Some("2026-04-16")),
            ("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
        ]);
        let dep1 = ResolvedDepModels {
            source_name: "dep-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert(
                    "opus".to_string(),
                    pinned_match_alias("claude-opus-4-7", "Anthropic", &["claude-opus-*"], &[]),
                );
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "dep-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert(
                    "opus".to_string(),
                    auto_alias("Anthropic", &["claude-opus-*"], &[]),
                );
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let _merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag, Some(&cache));
        let warnings = diag.drain();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .message
                .contains("both resolve to claude-opus-4-7")
        );
    }

    #[test]
    fn merge_dep_conflict_without_cache_uses_old_format() {
        let dep1 = ResolvedDepModels {
            source_name: "dep-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("a"), "model-a"));
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "dep-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("b"), "model-b"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let _merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag, None);
        let warnings = diag.drain();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].message,
            "model alias `custom` defined by both `dep-a` and `dep-b` — using dep-a (declared first)\n  → add [models.custom] to your mars.toml to resolve explicitly"
        );
    }

    #[test]
    fn merge_dep_three_way_conflict_warns_each_loser_against_first_winner() {
        let dep1 = ResolvedDepModels {
            source_name: "pkg-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("a"), "model-a"));
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "pkg-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("b"), "model-b"));
                m
            },
        };
        let dep3 = ResolvedDepModels {
            source_name: "pkg-c".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("c"), "model-c"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[dep1, dep2, dep3], &mut diag, None);

        assert_eq!(
            merged.get("custom").unwrap().spec,
            ModelSpec::Pinned {
                model: "model-a".to_string(),
                provider: None
            }
        );

        let warnings = diag.drain();
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            warnings[0].message,
            "model alias `custom` defined by both `pkg-a` and `pkg-b` — using pkg-a (declared first)\n  → add [models.custom] to your mars.toml to resolve explicitly"
        );
        assert_eq!(
            warnings[1].message,
            "model alias `custom` defined by both `pkg-a` and `pkg-c` — using pkg-a (declared first)\n  → add [models.custom] to your mars.toml to resolve explicitly"
        );
    }

    #[test]
    fn merge_consumer_override_suppresses_dep_conflict_warning() {
        let mut consumer = IndexMap::new();
        consumer.insert(
            "custom".to_string(),
            pinned_alias(Some("consumer"), "consumer-model"),
        );

        let dep1 = ResolvedDepModels {
            source_name: "pkg-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("a"), "model-a"));
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "pkg-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("b"), "model-b"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&consumer, &[dep1, dep2], &mut diag, None);

        assert_eq!(
            merged.get("custom").unwrap().spec,
            ModelSpec::Pinned {
                model: "consumer-model".to_string(),
                provider: None
            }
        );
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn merge_dep_conflicts_are_non_blocking() {
        let dep1 = ResolvedDepModels {
            source_name: "pkg-a".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("a"), "model-a"));
                m
            },
        };
        let dep2 = ResolvedDepModels {
            source_name: "pkg-b".to_string(),
            models: {
                let mut m = IndexMap::new();
                m.insert("custom".to_string(), pinned_alias(Some("b"), "model-b"));
                m.insert("extra".to_string(), pinned_alias(Some("b"), "model-extra"));
                m
            },
        };

        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag, None);

        assert!(merged.contains_key("opus"));
        assert_eq!(
            merged.get("custom").unwrap().spec,
            ModelSpec::Pinned {
                model: "model-a".to_string(),
                provider: None
            }
        );
        assert_eq!(
            merged.get("extra").unwrap().spec,
            ModelSpec::Pinned {
                model: "model-extra".to_string(),
                provider: None
            }
        );
        assert_eq!(diag.drain().len(), 1);
    }

    // -- resolve_all tests --

    #[test]
    fn resolve_all_pinned() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "fast".to_string(),
            pinned_alias(Some("claude"), "claude-haiku-4-5"),
        );

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("fast").unwrap();
        assert_eq!(entry.model_id, "claude-haiku-4-5");
        assert_eq!(entry.provider, "anthropic");
    }

    #[test]
    fn resolve_all_copies_alias_defaults() {
        let mut aliases = IndexMap::new();
        let mut alias = pinned_alias(Some("claude"), "claude-haiku-4-5");
        alias.default_effort = Some("medium".to_string());
        alias.autocompact = Some(30);
        aliases.insert("fast".to_string(), alias);

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("fast").unwrap();
        assert_eq!(entry.default_effort.as_deref(), Some("medium"));
        assert_eq!(entry.autocompact, Some(30));
    }

    #[test]
    fn resolve_all_pinned_with_provider() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "fast".to_string(),
            ModelAlias {
                harness: None,
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::Pinned {
                    model: "gpt-5.3-codex".to_string(),
                    provider: Some("openai".to_string()),
                },
            },
        );

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("fast").unwrap();
        assert_eq!(entry.model_id, "gpt-5.3-codex");
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.harness_candidates, vec!["codex", "opencode"]);
    }

    #[test]
    fn resolve_all_pinned_auto_detect_harness() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            ModelAlias {
                harness: None,
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::Pinned {
                    model: "claude-opus-4-6".to_string(),
                    provider: Some("anthropic".to_string()),
                },
            },
        );

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("opus").unwrap();
        assert_eq!(entry.model_id, "claude-opus-4-6");
        assert_eq!(entry.provider, "anthropic");

        let installed = harness::detect_installed_harnesses();
        let expected_harness = harness::resolve_harness_for_provider("anthropic", &installed);
        let expected_source = if expected_harness.is_some() {
            HarnessSource::AutoDetected
        } else {
            HarnessSource::Unavailable
        };

        assert_eq!(entry.harness, expected_harness);
        assert_eq!(entry.harness_source, expected_source);
    }

    #[test]
    fn resolve_all_auto_detect_harness() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "gpt".to_string(),
            ModelAlias {
                harness: None,
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::AutoResolve {
                    provider: "openai".to_string(),
                    match_patterns: vec!["gpt-5*".to_string()],
                    exclude_patterns: vec![],
                },
            },
        );
        let cache = make_cache(vec![("gpt-5", "OpenAI", Some("2025-06-01"))]);

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("gpt").unwrap();
        assert_eq!(entry.model_id, "gpt-5");
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.harness_candidates, vec!["codex", "opencode"]);
        match entry.harness_source {
            HarnessSource::AutoDetected => assert!(entry.harness.is_some()),
            HarnessSource::Unavailable => assert!(entry.harness.is_none()),
            HarnessSource::Explicit => panic!("unexpected explicit harness source"),
        }
    }

    #[test]
    fn resolve_all_unavailable_harness_still_included() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            ModelAlias {
                harness: Some("missing-harness-xyz".to_string()),
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::Pinned {
                    model: "claude-opus-4-6".to_string(),
                    provider: None,
                },
            },
        );

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        let entry = resolved.get("opus").unwrap();
        assert_eq!(entry.model_id, "claude-opus-4-6");
        assert_eq!(entry.provider, "anthropic");
        assert_eq!(entry.harness.as_deref(), Some("missing-harness-xyz"));
        assert_eq!(entry.harness_source, HarnessSource::Unavailable);
    }

    #[test]
    fn resolve_all_empty_cache_omits_unresolvable() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            ModelAlias {
                harness: Some("claude".to_string()),
                description: None,
                default_effort: None,
                autocompact: None,
                spec: ModelSpec::AutoResolve {
                    provider: "Anthropic".to_string(),
                    match_patterns: vec!["claude-opus-*".to_string()],
                    exclude_patterns: vec![],
                },
            },
        );
        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        // No cache → auto-resolve can't match → alias omitted from results
        assert!(!resolved.contains_key("opus"));
    }

    #[test]
    fn resolve_all_pinned_with_match_uses_model_field() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            pinned_match_alias("claude-opus-4-6", "Anthropic", &["claude-opus-*"], &[]),
        );
        let cache = make_cache(vec![
            ("claude-opus-4-7", "Anthropic", Some("2026-04-16")),
            ("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
        ]);

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_all(&aliases, &cache, &mut diag);
        assert_eq!(resolved.get("opus").unwrap().model_id, "claude-opus-4-6");
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn resolve_one_scopes_diagnostics_to_requested_alias() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            pinned_match_alias("claude-opus-4-6", "Anthropic", &["claude-opus-*"], &[]),
        );
        aliases.insert(
            "sonnet".to_string(),
            pinned_match_alias("claude-sonnet-4-5", "Anthropic", &["claude-sonnet-*"], &[]),
        );
        let cache = make_cache(vec![
            ("claude-opus-4-7", "Anthropic", Some("2026-04-16")),
            ("claude-sonnet-4-7", "Anthropic", Some("2026-04-16")),
        ]);

        let mut diag = DiagnosticCollector::new();
        let resolved = resolve_one("opus", &aliases, &cache, &mut diag).unwrap();
        assert_eq!(resolved.name, "opus");
        assert!(diag.drain().is_empty());
    }

    fn make_resolved_alias(name: &str) -> ResolvedAlias {
        ResolvedAlias {
            name: name.to_string(),
            model_id: format!("model-{name}"),
            provider: "openai".to_string(),
            harness: Some("codex".to_string()),
            harness_source: HarnessSource::Explicit,
            harness_candidates: vec!["codex".to_string()],
            description: None,
            default_effort: None,
            autocompact: None,
        }
    }

    #[test]
    fn filter_by_visibility_include_mode_keeps_matches_only() {
        let mut aliases = IndexMap::new();
        aliases.insert("opus".to_string(), make_resolved_alias("opus"));
        aliases.insert("sonnet".to_string(), make_resolved_alias("sonnet"));
        aliases.insert("gpt-5".to_string(), make_resolved_alias("gpt-5"));

        let filtered = filter_by_visibility(
            aliases,
            &crate::config::ModelVisibility {
                include: Some(vec!["opus*".to_string(), "gpt-*".to_string()]),
                exclude: None,
            },
        );

        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains_key("opus"));
        assert!(filtered.contains_key("gpt-5"));
        assert!(!filtered.contains_key("sonnet"));
    }

    #[test]
    fn filter_by_visibility_exclude_mode_removes_matches() {
        let mut aliases = IndexMap::new();
        aliases.insert("opus".to_string(), make_resolved_alias("opus"));
        aliases.insert("test-opus".to_string(), make_resolved_alias("test-opus"));
        aliases.insert(
            "deprecated-gpt".to_string(),
            make_resolved_alias("deprecated-gpt"),
        );

        let filtered = filter_by_visibility(
            aliases,
            &crate::config::ModelVisibility {
                include: None,
                exclude: Some(vec!["test-*".to_string(), "deprecated-*".to_string()]),
            },
        );

        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("opus"));
        assert!(!filtered.contains_key("test-opus"));
        assert!(!filtered.contains_key("deprecated-gpt"));
    }

    #[test]
    fn filter_by_visibility_empty_config_returns_all() {
        let mut aliases = IndexMap::new();
        aliases.insert("opus".to_string(), make_resolved_alias("opus"));
        aliases.insert("sonnet".to_string(), make_resolved_alias("sonnet"));
        let filtered = filter_by_visibility(aliases, &crate::config::ModelVisibility::default());
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains_key("opus"));
        assert!(filtered.contains_key("sonnet"));
    }

    #[test]
    fn resolve_model_and_provider_pinned_explicit_provider() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: Some("anthropic".to_string()),
            },
        };
        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let resolved = resolve_model_and_provider(&alias, &cache).unwrap();
        assert_eq!(
            resolved,
            ("claude-opus-4-6".to_string(), "anthropic".to_string())
        );
    }

    #[test]
    fn resolve_model_and_provider_pinned_inferred() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: None,
            },
        };
        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let resolved = resolve_model_and_provider(&alias, &cache).unwrap();
        assert_eq!(
            resolved,
            ("claude-opus-4-6".to_string(), "anthropic".to_string())
        );
    }

    #[test]
    fn resolve_model_and_provider_pinned_unknown() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "my-custom-model".to_string(),
                provider: None,
            },
        };
        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let resolved = resolve_model_and_provider(&alias, &cache).unwrap();
        assert_eq!(
            resolved,
            ("my-custom-model".to_string(), "unknown".to_string())
        );
    }

    #[test]
    fn resolve_model_and_provider_auto_resolve() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::AutoResolve {
                provider: "openai".to_string(),
                match_patterns: vec!["gpt-5*".to_string()],
                exclude_patterns: vec![],
            },
        };
        let cache = make_cache(vec![
            ("gpt-4o", "OpenAI", Some("2024-06-01")),
            ("gpt-5", "OpenAI", Some("2025-06-01")),
        ]);

        let resolved = resolve_model_and_provider(&alias, &cache).unwrap();
        assert_eq!(resolved, ("gpt-5".to_string(), "openai".to_string()));
    }

    #[test]
    fn resolve_harness_explicit_installed() {
        let alias = ModelAlias {
            harness: Some("claude".to_string()),
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: None,
            },
        };
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();

        let resolved = resolve_harness(&alias, "anthropic", &installed);
        assert_eq!(
            resolved,
            (Some("claude".to_string()), HarnessSource::Explicit)
        );
    }

    #[test]
    fn resolve_harness_explicit_not_installed() {
        let alias = ModelAlias {
            harness: Some("claude".to_string()),
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: None,
            },
        };
        let installed = HashSet::new();

        let resolved = resolve_harness(&alias, "anthropic", &installed);
        assert_eq!(
            resolved,
            (Some("claude".to_string()), HarnessSource::Unavailable)
        );
    }

    #[test]
    fn resolve_harness_auto_detected() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: Some("anthropic".to_string()),
            },
        };
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();

        let resolved = resolve_harness(&alias, "anthropic", &installed);
        assert_eq!(
            resolved,
            (Some("claude".to_string()), HarnessSource::AutoDetected)
        );
    }

    #[test]
    fn resolve_harness_unavailable() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "claude-opus-4-6".to_string(),
                provider: Some("anthropic".to_string()),
            },
        };
        let installed = HashSet::new();

        let resolved = resolve_harness(&alias, "anthropic", &installed);
        assert_eq!(resolved, (None, HarnessSource::Unavailable));
    }

    #[test]
    fn resolve_harness_unavailable_no_provider_match() {
        let alias = ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: ModelSpec::Pinned {
                model: "my-custom-model".to_string(),
                provider: Some("unknown".to_string()),
            },
        };
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();

        let resolved = resolve_harness(&alias, "unknown", &installed);
        assert_eq!(resolved, (None, HarnessSource::Unavailable));
    }

    // -- serde roundtrip tests --

    #[test]
    fn harness_source_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&HarnessSource::Explicit).unwrap(),
            "\"explicit\""
        );
        assert_eq!(
            serde_json::to_string(&HarnessSource::AutoDetected).unwrap(),
            "\"auto_detected\""
        );
        assert_eq!(
            serde_json::to_string(&HarnessSource::Unavailable).unwrap(),
            "\"unavailable\""
        );
    }

    #[test]
    fn model_alias_pinned_toml_roundtrip_backwards_compat_harness() {
        let toml_str = r#"
[models.fast]
harness = "claude"
model = "claude-haiku-4-5"
description = "Fast and cheap"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("fast").unwrap();
        assert_eq!(
            alias.spec,
            ModelSpec::Pinned {
                model: "claude-haiku-4-5".to_string(),
                provider: None
            }
        );
        assert_eq!(alias.harness.as_deref(), Some("claude"));
        assert_eq!(alias.description.as_deref(), Some("Fast and cheap"));

        let json = serde_json::to_string(alias).unwrap();
        let roundtripped: ModelAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, *alias);
    }

    #[test]
    fn model_alias_pinned_toml_roundtrip_without_harness() {
        let toml_str = r#"
[models.fast]
model = "claude-haiku-4-5"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("fast").unwrap();
        assert_eq!(alias.harness, None);
        assert_eq!(
            alias.spec,
            ModelSpec::Pinned {
                model: "claude-haiku-4-5".to_string(),
                provider: None
            }
        );

        let json = serde_json::to_string(alias).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("harness").is_none());
        assert!(value.get("provider").is_none());
        let roundtripped: ModelAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, *alias);
    }

    #[test]
    fn model_alias_pinned_toml_roundtrip_with_provider() {
        let toml_str = r#"
[models.fast]
model = "claude-haiku-4-5"
provider = "anthropic"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("fast").unwrap();
        assert_eq!(alias.harness, None);
        assert_eq!(
            alias.spec,
            ModelSpec::Pinned {
                model: "claude-haiku-4-5".to_string(),
                provider: Some("anthropic".to_string())
            }
        );

        let json = serde_json::to_string(alias).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value.get("provider").and_then(serde_json::Value::as_str),
            Some("anthropic")
        );
        let roundtripped: ModelAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, *alias);
    }

    #[test]
    fn model_alias_pinned_json_roundtrip_with_provider() {
        let json = r#"{
            "model": "gpt-5.3-codex",
            "provider": "openai"
        }"#;

        let alias: ModelAlias = serde_json::from_str(json).unwrap();
        assert_eq!(alias.harness, None);
        assert_eq!(alias.description, None);
        assert_eq!(
            alias.spec,
            ModelSpec::Pinned {
                model: "gpt-5.3-codex".to_string(),
                provider: Some("openai".to_string())
            }
        );

        let encoded = serde_json::to_string(&alias).unwrap();
        let roundtripped: ModelAlias = serde_json::from_str(&encoded).unwrap();
        assert_eq!(roundtripped, alias);
    }

    #[test]
    fn model_alias_auto_resolve_toml_roundtrip() {
        let toml_str = r#"
[models.opus]
harness = "claude"
provider = "Anthropic"
match = ["claude-opus-*"]
exclude = ["claude-opus-3*"]
description = "Best reasoning"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("opus").unwrap();
        assert_eq!(alias.harness.as_deref(), Some("claude"));
        match &alias.spec {
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                assert_eq!(provider, "Anthropic");
                assert_eq!(match_patterns, &["claude-opus-*"]);
                assert_eq!(exclude_patterns, &["claude-opus-3*"]);
            }
            _ => panic!("expected AutoResolve"),
        }
    }

    #[test]
    fn model_alias_model_and_match_toml_roundtrip() {
        let toml_str = r#"
[models.opus]
model = "claude-opus-4-6"
provider = "anthropic"
match = ["claude-opus-*"]
exclude = ["claude-opus-3*"]
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("opus").unwrap();
        match &alias.spec {
            ModelSpec::PinnedWithMatch {
                model,
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                assert_eq!(model, "claude-opus-4-6");
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(match_patterns, &["claude-opus-*"]);
                assert_eq!(exclude_patterns, &["claude-opus-3*"]);
            }
            _ => panic!("expected PinnedWithMatch"),
        }

        let json = serde_json::to_string(alias).unwrap();
        let roundtripped: ModelAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, *alias);
    }

    #[test]
    fn model_alias_model_with_exclude_without_match_errors() {
        let toml_str = r#"
[models.opus]
model = "claude-opus-4-7"
exclude = ["claude-opus-3*"]
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let err = toml::from_str::<Wrapper>(toml_str).unwrap_err().to_string();
        assert!(err.contains("must also include 'match'"));
    }

    #[test]
    fn model_alias_defaults_toml_roundtrip() {
        let toml_str = r#"
[models.opus]
provider = "Anthropic"
match = ["claude-opus-*"]
default_effort = "high"
autocompact = 25
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("opus").unwrap();
        assert_eq!(alias.default_effort.as_deref(), Some("high"));
        assert_eq!(alias.autocompact, Some(25));

        let json = serde_json::to_string(alias).unwrap();
        let roundtripped: ModelAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, *alias);
    }

    #[test]
    fn model_alias_empty_default_effort_treated_as_none() {
        let toml_str = r#"
[models.opus]
provider = "Anthropic"
match = ["claude-opus-*"]
default_effort = ""
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            models: IndexMap<String, ModelAlias>,
        }

        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        let alias = parsed.models.get("opus").unwrap();
        assert_eq!(alias.default_effort, None);
    }

    #[test]
    fn model_alias_invalid_default_effort_errors() {
        let toml_str = r#"
[models.opus]
provider = "Anthropic"
match = ["claude-opus-*"]
default_effort = "maximum"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let err = toml::from_str::<Wrapper>(toml_str).unwrap_err().to_string();
        assert!(err.contains("invalid default_effort"));
        assert!(err.contains("accepted values"));
    }

    #[test]
    fn model_alias_autocompact_out_of_range_errors() {
        let toml_str = r#"
[models.opus]
provider = "Anthropic"
match = ["claude-opus-*"]
autocompact = 101
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let err = toml::from_str::<Wrapper>(toml_str).unwrap_err().to_string();
        assert!(err.contains("out of range 1-100"));
    }

    #[test]
    fn model_alias_autocompact_boolean_errors() {
        let toml_str = r#"
[models.opus]
provider = "Anthropic"
match = ["claude-opus-*"]
autocompact = true
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let err = toml::from_str::<Wrapper>(toml_str).unwrap_err().to_string();
        assert!(err.contains("autocompact must be an integer 1-100"));
    }

    #[test]
    fn model_alias_both_model_and_match_is_hybrid_pinned() {
        let toml_str = r#"
[models.bad]
harness = "claude"
model = "some-model"
match = ["pattern-*"]
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let result = toml::from_str::<Wrapper>(toml_str).unwrap();
        let alias = result.models.get("bad").unwrap();
        match &alias.spec {
            ModelSpec::PinnedWithMatch {
                model,
                match_patterns,
                ..
            } => {
                assert_eq!(model, "some-model");
                assert_eq!(match_patterns, &["pattern-*"]);
            }
            _ => panic!("expected pinned-with-match alias"),
        }
    }

    #[test]
    fn model_alias_neither_model_nor_match_errors() {
        let toml_str = r#"
[models.bad]
harness = "claude"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let result = toml::from_str::<Wrapper>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn infer_provider_from_model_id_detects_known_prefixes() {
        assert_eq!(
            infer_provider_from_model_id("claude-opus-4-6"),
            Some("anthropic")
        );
        assert_eq!(
            infer_provider_from_model_id("gpt-5.3-codex"),
            Some("openai")
        );
        assert_eq!(
            infer_provider_from_model_id("gemini-2.5-pro"),
            Some("google")
        );
        assert_eq!(
            infer_provider_from_model_id("llama-4-maverick"),
            Some("meta")
        );
        assert_eq!(infer_provider_from_model_id("o1-preview"), Some("openai"));
        assert_eq!(infer_provider_from_model_id("o3-mini"), Some("openai"));
        assert_eq!(infer_provider_from_model_id("o4-mini"), Some("openai"));
        assert_eq!(
            infer_provider_from_model_id("codex-mini-latest"),
            Some("openai")
        );
        assert_eq!(
            infer_provider_from_model_id("mistral-large"),
            Some("mistral")
        );
        assert_eq!(
            infer_provider_from_model_id("codestral-latest"),
            Some("mistral")
        );
        assert_eq!(
            infer_provider_from_model_id("deepseek-chat"),
            Some("deepseek")
        );
        assert_eq!(
            infer_provider_from_model_id("command-r-plus"),
            Some("cohere")
        );
    }

    #[test]
    fn infer_provider_from_model_id_returns_none_for_unknown_model() {
        assert_eq!(infer_provider_from_model_id("unknown-model"), None);
    }

    #[test]
    fn infer_provider_from_model_id_returns_none_for_empty_string() {
        assert_eq!(infer_provider_from_model_id(""), None);
    }

    #[test]
    fn infer_provider_from_model_id_is_case_insensitive() {
        assert_eq!(
            infer_provider_from_model_id("CLAUDE-OPUS-4-6"),
            Some("anthropic")
        );
        assert_eq!(
            infer_provider_from_model_id("GPT-5.3-codex"),
            Some("openai")
        );
        assert_eq!(
            infer_provider_from_model_id("CoDeStRaL-latest"),
            Some("mistral")
        );
    }

    #[allow(unused_unsafe)]
    fn env_set(key: &str, value: &str) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    #[allow(unused_unsafe)]
    fn env_remove(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    struct EnvVarGuard {
        key: String,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            env_set(key, value);
            Self {
                key: key.to_string(),
                prev,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                env_set(&self.key, prev);
            } else {
                env_remove(&self.key);
            }
        }
    }

    fn sample_catalog_json() -> serde_json::Value {
        serde_json::json!({
            "openai": {
                "models": {
                    "gpt-5": {
                        "id": "gpt-5",
                        "name": "GPT-5",
                        "release_date": "2025-06-01",
                        "limit": {
                            "context": 400000,
                            "output": 128000
                        }
                    }
                }
            },
            "anthropic": {
                "models": {
                    "claude-sonnet-4-5": {
                        "id": "claude-sonnet-4-5",
                        "name": "Claude Sonnet 4.5",
                        "release_date": "2025-03-01"
                    }
                }
            }
        })
    }

    fn sample_cached_model(id: &str) -> CachedModel {
        CachedModel {
            id: id.to_string(),
            provider: "OpenAI".to_string(),
            release_date: None,
            description: None,
            context_window: None,
            max_output: None,
        }
    }

    fn write_cache_state(mars_dir: &std::path::Path, models: Vec<CachedModel>, fetched_at: &str) {
        write_cache(
            mars_dir,
            &ModelsCache {
                models,
                fetched_at: Some(fetched_at.to_string()),
            },
        )
        .expect("failed to write cache fixture");
    }

    fn write_raw_cache_file(mars_dir: &std::path::Path, raw: &str) {
        std::fs::create_dir_all(mars_dir).expect("failed to create mars dir");
        std::fs::write(mars_dir.join(CACHE_FILE), raw).expect("failed to write raw cache");
    }

    fn stale_timestamp() -> String {
        now_unix_secs_value().saturating_sub(48 * 3600).to_string()
    }

    fn fresh_timestamp() -> String {
        now_unix_secs_value().saturating_sub(60).to_string()
    }

    fn assert_model_cache_unavailable(
        result: Result<(ModelsCache, RefreshOutcome), MarsError>,
        reason_contains: &str,
    ) {
        match result {
            Err(MarsError::ModelCacheUnavailable { reason }) => {
                assert!(
                    reason.contains(reason_contains),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected ModelCacheUnavailable, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn ensure_fresh_1_missing_cache_offline_errors() {
        let mars = tempdir().unwrap();
        let _offline = EnvVarGuard::set("MARS_OFFLINE", "1");

        let result = ensure_fresh(mars.path(), 24, RefreshMode::Auto);
        assert_model_cache_unavailable(result, "MARS_OFFLINE is set");
    }

    #[test]
    #[serial]
    fn ensure_fresh_2_missing_cache_auto_fetch_failure_errors() {
        let mars = tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(500).body("server error");
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let result = ensure_fresh(mars.path(), 24, RefreshMode::Auto);
        assert_model_cache_unavailable(result, "automatic refresh failed");
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn ensure_fresh_3_stale_usable_offline_returns_stale() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Offline).unwrap();
        assert_eq!(cache.models.len(), 1);
        assert_eq!(cache.models[0].id, "stale-model");
        assert_eq!(outcome, RefreshOutcome::Offline);
    }

    #[test]
    #[serial]
    fn ensure_fresh_4_fresh_auto_skips_http() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("fresh-model")],
            &fresh_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert_eq!(outcome, RefreshOutcome::AlreadyFresh);
        assert_eq!(mock.hits(), 0);
    }

    #[test]
    #[serial]
    fn ensure_fresh_5_stale_auto_success_refreshes() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("old-model")],
            &stale_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert!(matches!(
            outcome,
            RefreshOutcome::Refreshed { models_count } if models_count == 2
        ));
        assert_eq!(cache.models.len(), 2);
        assert!(!cache.models.is_empty());
        assert!(cache.fetched_at.is_some());
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_6_stale_auto_fetch_failure_falls_back() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(500).body("server error");
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert_eq!(cache.models[0].id, "stale-model");
        assert!(matches!(
            outcome,
            RefreshOutcome::StaleFallback { reason } if reason.contains("fetch failed")
        ));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_7_stale_auto_empty_catalog_falls_back() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(serde_json::json!({}));
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert_eq!(cache.models[0].id, "stale-model");
        assert!(matches!(
            outcome,
            RefreshOutcome::StaleFallback { reason } if reason == "API returned empty catalog"
        ));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_8_empty_cache_auto_refetches() {
        let mars = tempdir().unwrap();
        write_cache_state(mars.path(), Vec::new(), &fresh_timestamp());

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert!(!cache.models.is_empty());
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn ensure_fresh_9_empty_cache_offline_errors() {
        let mars = tempdir().unwrap();
        write_cache_state(mars.path(), Vec::new(), &fresh_timestamp());

        let result = ensure_fresh(mars.path(), 24, RefreshMode::Offline);
        assert_model_cache_unavailable(result, "--no-refresh-models was passed");
    }

    #[test]
    #[serial]
    fn ensure_fresh_10_corrupt_json_auto_refetches() {
        let mars = tempdir().unwrap();
        write_raw_cache_file(mars.path(), "{ not-json ");

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert!(!cache.models.is_empty());
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn ensure_fresh_11_corrupt_json_offline_errors() {
        let mars = tempdir().unwrap();
        write_raw_cache_file(mars.path(), "{ not-json ");

        let result = ensure_fresh(mars.path(), 24, RefreshMode::Offline);
        assert_model_cache_unavailable(result, "--no-refresh-models was passed");
    }

    #[test]
    fn read_cache_io_error_includes_operation_and_path() {
        let mars = tempdir().unwrap();
        let cache_path = mars.path().join(CACHE_FILE);
        std::fs::create_dir(&cache_path).unwrap();

        let err = read_cache(mars.path()).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("read models cache"),
            "error should include operation context: {msg}"
        );
        assert!(
            msg.contains(CACHE_FILE),
            "error should include cache path: {msg}"
        );
    }

    #[test]
    #[serial]
    fn ensure_fresh_12_ttl_zero_always_refetches() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("fresh-model")],
            &fresh_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache, outcome) = ensure_fresh(mars.path(), 0, RefreshMode::Auto).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_13_unparseable_fetched_at_is_stale() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            "not-a-timestamp",
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_14_future_fetched_at_is_stale() {
        let mars = tempdir().unwrap();
        let future = now_unix_secs_value() + 3600;
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("future-model")],
            &future.to_string(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_15_offline_env_auto_fresh_returns_offline() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("fresh-model")],
            &fresh_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));
        let _offline = EnvVarGuard::set("MARS_OFFLINE", "1");

        let (_cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        assert_eq!(outcome, RefreshOutcome::Offline);
        assert_eq!(mock.hits(), 0);
    }

    #[test]
    #[serial]
    fn ensure_fresh_16_offline_env_zero_is_not_offline() {
        let _offline = EnvVarGuard::set("MARS_OFFLINE", "0");
        assert!(!is_mars_offline());
        assert_eq!(resolve_refresh_mode(false), RefreshMode::Auto);
    }

    #[test]
    #[serial]
    fn ensure_fresh_17_offline_env_truthy_is_offline() {
        let _offline = EnvVarGuard::set("MARS_OFFLINE", " TRUE ");
        assert!(is_mars_offline());
        assert_eq!(resolve_refresh_mode(false), RefreshMode::Auto);
    }

    #[test]
    #[serial]
    fn ensure_fresh_18_force_ignores_offline_env() {
        let mars = tempdir().unwrap();
        let _offline = EnvVarGuard::set("MARS_OFFLINE", "1");

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(sample_catalog_json());
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache, outcome) = ensure_fresh(mars.path(), 24, RefreshMode::Force).unwrap();
        assert!(matches!(outcome, RefreshOutcome::Refreshed { .. }));
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_19_concurrent_auto_refresh_hits_api_once() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let path = Arc::new(mars.path().to_path_buf());
        let path_a = Arc::clone(&path);
        let path_b = Arc::clone(&path);
        let fetch_hits = Arc::new(AtomicUsize::new(0));
        let (fetch_started_tx, fetch_started_rx) = mpsc::channel::<()>();
        let (release_fetch_tx, release_fetch_rx) = mpsc::channel::<()>();

        let fetch_hits_a = Arc::clone(&fetch_hits);
        let t1 = thread::spawn(move || {
            ensure_fresh_with_fetcher(&path_a, 24, RefreshMode::Auto, move || {
                fetch_hits_a.fetch_add(1, Ordering::SeqCst);
                fetch_started_tx.send(()).unwrap();
                release_fetch_rx.recv().unwrap();
                Ok(vec![sample_cached_model("fresh-model")])
            })
            .unwrap()
            .1
        });

        fetch_started_rx.recv().unwrap();

        let fetch_hits_b = Arc::clone(&fetch_hits);
        let t2 = thread::spawn(move || {
            ensure_fresh_with_fetcher(&path_b, 24, RefreshMode::Auto, move || {
                fetch_hits_b.fetch_add(1, Ordering::SeqCst);
                Ok(vec![sample_cached_model("unexpected-second-refresh")])
            })
            .unwrap()
            .1
        });

        release_fetch_tx.send(()).unwrap();

        let outcome_a = t1.join().unwrap();
        let outcome_b = t2.join().unwrap();

        let outcomes = [outcome_a, outcome_b];
        let refreshed = outcomes
            .iter()
            .filter(|o| matches!(o, RefreshOutcome::Refreshed { .. }))
            .count();
        let already_fresh = outcomes
            .iter()
            .filter(|o| matches!(o, RefreshOutcome::AlreadyFresh))
            .count();

        assert_eq!(refreshed, 1);
        assert_eq!(already_fresh, 1);
        assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_20_failed_fetch_cooldown_coalesces_sequential_calls() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(500).body("server error");
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache_a, outcome_a) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        let (_cache_b, outcome_b) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();

        assert!(matches!(
            outcome_a,
            RefreshOutcome::StaleFallback { reason } if reason.contains("fetch failed")
        ));
        assert_eq!(
            outcome_b,
            RefreshOutcome::StaleFallback {
                reason: FETCH_FAIL_COOLDOWN_REASON.to_string()
            }
        );
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    #[serial]
    fn ensure_fresh_21_empty_catalog_cooldown_coalesces_sequential_calls() {
        let mars = tempdir().unwrap();
        write_cache_state(
            mars.path(),
            vec![sample_cached_model("stale-model")],
            &stale_timestamp(),
        );

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api.json");
            then.status(200).json_body(serde_json::json!({
                "openai": {
                    "models": {}
                }
            }));
        });
        let _api = EnvVarGuard::set("MARS_MODELS_API_URL", &server.url("/api.json"));

        let (_cache_a, outcome_a) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();
        let (_cache_b, outcome_b) = ensure_fresh(mars.path(), 24, RefreshMode::Auto).unwrap();

        assert!(matches!(
            outcome_a,
            RefreshOutcome::StaleFallback { reason } if reason.contains("API returned empty catalog")
        ));
        assert_eq!(
            outcome_b,
            RefreshOutcome::StaleFallback {
                reason: FETCH_FAIL_COOLDOWN_REASON.to_string()
            }
        );
        assert_eq!(mock.hits(), 1);
    }

    #[test]
    fn load_models_cache_ttl_defaults_to_24_when_config_missing() {
        let project = tempdir().unwrap();
        let ctx = crate::types::MarsContext::for_test(
            project.path().to_path_buf(),
            project.path().join(".agents"),
        );
        assert_eq!(load_models_cache_ttl(&ctx), 24);
    }

    #[test]
    fn load_models_cache_ttl_reads_config_value() {
        let project = tempdir().unwrap();
        std::fs::write(
            project.path().join("mars.toml"),
            "[settings]\nmodels_cache_ttl_hours = 48\n",
        )
        .unwrap();
        let ctx = crate::types::MarsContext::for_test(
            project.path().to_path_buf(),
            project.path().join(".agents"),
        );
        assert_eq!(load_models_cache_ttl(&ctx), 48);
    }
}
