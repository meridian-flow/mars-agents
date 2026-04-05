//! Model catalog — two-mode aliases (pinned + auto-resolve),
//! dependency-tree config merge, and models cache lifecycle.
//!
//! Model aliases map short names (opus, sonnet, codex) to concrete model IDs.
//! Two modes:
//! - **Pinned**: explicit model ID, no resolution needed.
//! - **AutoResolve**: pattern-based resolution against a cached model catalog.
//!
//! Merge precedence: consumer > deps (declaration order).

use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;

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
    /// Pattern-based resolution against models cache.
    AutoResolve {
        provider: String,
        match_patterns: Vec<String>,
        exclude_patterns: Vec<String>,
    },
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

        let has_model = raw.model.is_some();
        let has_match = raw.match_patterns.is_some();

        if has_model && has_match {
            return Err(serde::de::Error::custom(
                "model alias cannot have both 'model' and 'match' — use one or the other",
            ));
        }

        let spec = if let Some(model) = raw.model {
            ModelSpec::Pinned {
                model,
                provider: raw.provider,
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
        Err(e) => Err(MarsError::Io(e)),
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
    let url = "https://models.dev/api.json";
    let response = ureq::get(url).call().map_err(|e| MarsError::Http {
        url: url.to_string(),
        status: 0,
        message: format!("failed to fetch models catalog: {e}"),
    })?;
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|e| MarsError::Http {
            url: url.to_string(),
            status: 0,
            message: format!("failed to read response body: {e}"),
        })?;
    let raw: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| crate::error::ConfigError::Invalid {
            message: format!("failed to parse models API response: {e}"),
        })?;

    parse_models_dev_catalog(&raw)
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
/// 5. Sort by newest release_date, then shortest ID
/// 6. Pick first
pub fn auto_resolve(
    provider: &str,
    match_patterns: &[String],
    exclude_patterns: &[String],
    cache: &ModelsCache,
) -> Option<String> {
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

    // Sort: newest release_date first, then shortest ID (tiebreaker)
    candidates.sort_by(|a, b| {
        let date_cmp = b
            .release_date
            .as_deref()
            .unwrap_or("")
            .cmp(a.release_date.as_deref().unwrap_or(""));
        date_cmp.then_with(|| a.id.len().cmp(&b.id.len()))
    });

    candidates.first().map(|m| m.id.clone())
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
) -> IndexMap<String, ModelAlias> {
    let mut merged = IndexMap::new();
    let builtins = builtin_aliases();

    // Layer 0 (lowest): builtins
    for (name, alias) in &builtins {
        merged.insert(name.clone(), alias.clone());
    }

    // Track which aliases were set by a dep (vs builtin)
    let mut dep_provided: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Layer 1: dependencies (override builtins silently, first dep wins on conflicts)
    for dep in deps {
        for (name, alias) in &dep.models {
            if consumer.contains_key(name) {
                // Consumer will override — skip dep's version silently
                continue;
            }
            if dep_provided.contains(name) {
                // Two deps define same alias — first dep wins, warn
                diag.warn_with_context(
                    "model-alias-conflict",
                    format!(
                        "model alias `{name}` defined by both `{}` and earlier dependency — using earlier definition",
                        dep.source_name
                    ),
                    dep.source_name.clone(),
                );
            } else {
                // Override builtin or insert new
                merged.insert(name.clone(), alias.clone());
                dep_provided.insert(name.clone());
            }
        }
    }

    // Layer 2 (highest): consumer config
    for (name, alias) in consumer {
        merged.insert(name.clone(), alias.clone());
    }

    merged
}

/// Resolve all aliases in a merged config, returning (alias → resolved_model_id).
/// For pinned aliases, returns the model ID directly. For auto-resolve, runs
/// the resolution algorithm against the cache. Unresolvable aliases are omitted.
pub fn resolve_all(
    aliases: &IndexMap<String, ModelAlias>,
    cache: &ModelsCache,
) -> IndexMap<String, String> {
    let mut resolved = IndexMap::new();

    for (name, alias) in aliases {
        let model_id = match &alias.spec {
            ModelSpec::Pinned { model, provider: _ } => Some(model.clone()),
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => auto_resolve(provider, match_patterns, exclude_patterns, cache),
        };
        if let Some(id) = model_id {
            resolved.insert(name.clone(), id);
        }
    }

    resolved
}

/// Best-effort provider inference from model ID prefixes.
/// Returns None for unrecognized patterns.
#[allow(dead_code)]
fn infer_provider_from_model_id(model_id: &str) -> Option<&'static str> {
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

    // -- merge_model_config tests --

    fn pinned_alias(harness: Option<&str>, model: &str) -> ModelAlias {
        ModelAlias {
            harness: harness.map(|h| h.to_string()),
            description: None,
            spec: ModelSpec::Pinned {
                model: model.to_string(),
                provider: None,
            },
        }
    }

    #[test]
    fn merge_empty_returns_builtins() {
        let mut diag = DiagnosticCollector::new();
        let merged = merge_model_config(&IndexMap::new(), &[], &mut diag);
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
        let merged = merge_model_config(&consumer, &[], &mut diag);
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
        let merged = merge_model_config(&IndexMap::new(), &[dep], &mut diag);
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
        let merged = merge_model_config(&consumer, &[dep], &mut diag);
        assert_eq!(
            merged.get("opus").unwrap().spec,
            ModelSpec::Pinned {
                model: "consumer-opus".to_string(),
                provider: None
            }
        );
    }

    #[test]
    fn merge_dep_conflict_warns() {
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
        let merged = merge_model_config(&IndexMap::new(), &[dep1, dep2], &mut diag);
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
    }

    // -- resolve_all tests --

    #[test]
    fn resolve_all_pinned() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "fast".to_string(),
            pinned_alias(Some("claude"), "claude-haiku-4"),
        );

        let cache = ModelsCache {
            models: Vec::new(),
            fetched_at: None,
        };

        let resolved = resolve_all(&aliases, &cache);
        assert_eq!(resolved.get("fast").unwrap(), "claude-haiku-4");
    }

    #[test]
    fn resolve_all_empty_cache_omits_unresolvable() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            ModelAlias {
                harness: Some("claude".to_string()),
                description: None,
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

        let resolved = resolve_all(&aliases, &cache);
        // No cache → auto-resolve can't match → alias omitted from results
        assert!(!resolved.contains_key("opus"));
    }

    // -- serde roundtrip tests --

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
    fn model_alias_both_model_and_match_errors() {
        let toml_str = r#"
[models.bad]
harness = "claude"
model = "some-model"
match = ["pattern-*"]
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[expect(dead_code)]
            models: IndexMap<String, ModelAlias>,
        }

        let result = toml::from_str::<Wrapper>(toml_str);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("both"));
    }

    #[test]
    fn model_alias_neither_model_nor_match_errors() {
        let toml_str = r#"
[models.bad]
harness = "claude"
"#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[expect(dead_code)]
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
    }

    #[test]
    fn infer_provider_from_model_id_returns_none_for_unknown_model() {
        assert_eq!(infer_provider_from_model_id("unknown-model"), None);
    }
}
