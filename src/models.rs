//! Model metadata: fetch from models.dev, cache locally, and resolve aliases.
//!
//! The cache lives at `<managed_root>/models-cache.json` and is refreshed
//! by `mars models refresh`. All reads go through the cache — no network
//! calls at list/resolve time.

use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::MarsError;
use crate::fs::atomic_write;

const API_URL: &str = "https://models.dev/api.json";
const CACHE_FILENAME: &str = "models-cache.json";

/// Per-model pricing (USD per million tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Context and output limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub output: u64,
}

/// Capabilities for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub tool_call: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub attachment: bool,
}

/// A cached model entry — the subset of models.dev data we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModel {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
    pub limits: ModelLimits,
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
}

/// The full cache file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsCache {
    /// ISO 8601 timestamp of last refresh.
    pub refreshed_at: String,
    /// Models keyed by `provider/model_id` for uniqueness.
    pub models: Vec<CachedModel>,
}

/// Model alias entry in mars.toml [models] section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelAlias {
    pub harness: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Map provider name to harness name.
fn provider_to_harness(provider: &str) -> &str {
    match provider {
        "anthropic" => "claude",
        "openai" => "codex",
        "google" => "opencode",
        _ => provider,
    }
}

/// Infer cost tier from input+output pricing.
pub fn cost_tier(cost: &Option<ModelCost>) -> &'static str {
    let Some(c) = cost else { return "?" };
    let avg = (c.input + c.output) / 2.0;
    if avg < 0.5 {
        "free"
    } else if avg < 2.0 {
        "low"
    } else if avg < 15.0 {
        "mid"
    } else if avg < 50.0 {
        "high"
    } else {
        "premium"
    }
}

/// Fetch models from models.dev API, filter to tool_call capable, and return
/// a `ModelsCache` ready to write.
pub fn fetch_models() -> Result<ModelsCache, MarsError> {
    let mut response = ureq::get(API_URL).call().map_err(|e| MarsError::Http {
        url: API_URL.to_string(),
        status: 0,
        message: e.to_string(),
    })?;

    let bytes = response
        .body_mut()
        .with_config()
        .limit(50 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| MarsError::Http {
            url: API_URL.to_string(),
            status: 0,
            message: format!("failed to read response: {e}"),
        })?;

    let body: HashMap<String, serde_json::Value> =
        serde_json::from_slice(&bytes).map_err(|e| MarsError::Http {
            url: API_URL.to_string(),
            status: 0,
            message: format!("failed to parse response: {e}"),
        })?;

    let mut models = Vec::new();

    for (provider_id, provider_val) in &body {
        let provider_obj = match provider_val.as_object() {
            Some(o) => o,
            None => continue,
        };
        let models_obj = match provider_obj
            .get("models")
            .and_then(|v: &serde_json::Value| v.as_object())
        {
            Some(o) => o,
            None => continue,
        };

        for (_model_key, model_val) in models_obj {
            let Some(model_obj) = model_val.as_object() else {
                continue;
            };

            // Filter: only models with tool_call capability
            let tool_call = model_obj
                .get("tool_call")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !tool_call {
                continue;
            }

            let id = model_obj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = model_obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();

            let cost = model_obj.get("cost").and_then(|v| {
                let obj = v.as_object()?;
                Some(ModelCost {
                    input: obj.get("input")?.as_f64()?,
                    output: obj.get("output")?.as_f64()?,
                    cache_read: obj.get("cache_read").and_then(|v| v.as_f64()),
                    cache_write: obj.get("cache_write").and_then(|v| v.as_f64()),
                })
            });

            let limit_obj = model_obj.get("limit").and_then(|v| v.as_object());
            let limits = ModelLimits {
                context: limit_obj
                    .and_then(|o| o.get("context"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output: limit_obj
                    .and_then(|o| o.get("output"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            };

            let reasoning = model_obj
                .get("reasoning")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let attachment = model_obj
                .get("attachment")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let release_date = model_obj
                .get("release_date")
                .and_then(|v| v.as_str())
                .map(String::from);

            let harness = provider_to_harness(provider_id).to_string();

            models.push(CachedModel {
                id,
                name,
                provider: provider_id.clone(),
                harness,
                cost,
                limits,
                capabilities: ModelCapabilities {
                    tool_call: true,
                    reasoning,
                    attachment,
                },
                release_date,
            });
        }
    }

    // Sort by provider then model id for stable output
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));

    let now = chrono_lite_now();

    Ok(ModelsCache {
        refreshed_at: now,
        models,
    })
}

/// Write cache to `<managed_root>/models-cache.json` atomically.
pub fn write_cache(managed_root: &Path, cache: &ModelsCache) -> Result<(), MarsError> {
    let path = managed_root.join(CACHE_FILENAME);
    let json = serde_json::to_string_pretty(cache).map_err(|e| MarsError::Http {
        url: String::new(),
        status: 0,
        message: format!("failed to serialize cache: {e}"),
    })?;
    atomic_write(&path, json.as_bytes())
}

/// Read cache from `<managed_root>/models-cache.json`.
pub fn read_cache(managed_root: &Path) -> Result<ModelsCache, MarsError> {
    let path = managed_root.join(CACHE_FILENAME);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MarsError::InvalidRequest {
                message: format!(
                    "no models cache found at {}. Run `mars models refresh` first.",
                    path.display()
                ),
            }
        } else {
            MarsError::Io(e)
        }
    })?;
    serde_json::from_str(&content).map_err(|e| MarsError::InvalidRequest {
        message: format!("failed to parse models cache: {e}"),
    })
}

/// Resolve an alias from the [models] config section.
pub fn resolve_alias<'a>(
    aliases: &'a IndexMap<String, ModelAlias>,
    alias: &str,
) -> Option<&'a ModelAlias> {
    aliases.get(alias)
}

/// Simple UTC timestamp without pulling in chrono.
fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Good enough for a cache timestamp
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_to_harness_mapping() {
        assert_eq!(provider_to_harness("anthropic"), "claude");
        assert_eq!(provider_to_harness("openai"), "codex");
        assert_eq!(provider_to_harness("google"), "opencode");
        assert_eq!(provider_to_harness("unknown"), "unknown");
    }

    #[test]
    fn cost_tier_levels() {
        assert_eq!(cost_tier(&None), "?");
        assert_eq!(
            cost_tier(&Some(ModelCost {
                input: 0.1,
                output: 0.4,
                cache_read: None,
                cache_write: None
            })),
            "free"
        );
        assert_eq!(
            cost_tier(&Some(ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: None,
                cache_write: None
            })),
            "low"
        );
        assert_eq!(
            cost_tier(&Some(ModelCost {
                input: 5.0,
                output: 15.0,
                cache_read: None,
                cache_write: None
            })),
            "mid"
        );
        assert_eq!(
            cost_tier(&Some(ModelCost {
                input: 15.0,
                output: 75.0,
                cache_read: None,
                cache_write: None
            })),
            "high"
        );
        assert_eq!(
            cost_tier(&Some(ModelCost {
                input: 150.0,
                output: 600.0,
                cache_read: None,
                cache_write: None
            })),
            "premium"
        );
    }

    #[test]
    fn cache_roundtrip() {
        let cache = ModelsCache {
            refreshed_at: "1234567890".to_string(),
            models: vec![CachedModel {
                id: "test-model".to_string(),
                name: "Test Model".to_string(),
                provider: "anthropic".to_string(),
                harness: "claude".to_string(),
                cost: Some(ModelCost {
                    input: 3.0,
                    output: 15.0,
                    cache_read: Some(0.3),
                    cache_write: None,
                }),
                limits: ModelLimits {
                    context: 200000,
                    output: 32000,
                },
                capabilities: ModelCapabilities {
                    tool_call: true,
                    reasoning: false,
                    attachment: true,
                },
                release_date: Some("2025-05-22".to_string()),
            }],
        };

        let json = serde_json::to_string(&cache).unwrap();
        let parsed: ModelsCache = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.models.len(), 1);
        assert_eq!(parsed.models[0].id, "test-model");
        assert_eq!(parsed.models[0].harness, "claude");
    }

    #[test]
    fn resolve_alias_works() {
        let mut aliases = IndexMap::new();
        aliases.insert(
            "opus".to_string(),
            ModelAlias {
                harness: "claude".to_string(),
                model: "claude-opus-4-6".to_string(),
                description: None,
            },
        );
        let result = resolve_alias(&aliases, "opus");
        assert!(result.is_some());
        assert_eq!(result.unwrap().model, "claude-opus-4-6");
        assert!(resolve_alias(&aliases, "nonexistent").is_none());
    }

    #[test]
    fn write_and_read_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = ModelsCache {
            refreshed_at: "1234567890".to_string(),
            models: vec![],
        };
        write_cache(dir.path(), &cache).unwrap();
        let loaded = read_cache(dir.path()).unwrap();
        assert_eq!(loaded.refreshed_at, "1234567890");
    }
}
