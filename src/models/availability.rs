use std::collections::{HashMap, HashSet};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityStatus {
    Runnable,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilitySource {
    HarnessInstalled,
    OpenCodeProbe,
    OpenCodeProbeNegative,
    OpenCodeProbeUnknown,
    NoHarness,
    Offline,
}

/// A runnable model path — one specific way to execute a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunnablePath {
    pub harness: String,
    pub mars_provider: String,
    pub harness_model_id: String,
}

/// Full availability assessment for a resolved model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelAvailability {
    pub status: AvailabilityStatus,
    pub source: AvailabilitySource,
    pub runnable_paths: Vec<RunnablePath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposedSlug {
    pub oc_provider: String,
    pub upstream_provider: Option<String>,
    pub model_part: String,
    pub full_slug: String,
}

#[derive(Debug, Clone, Default)]
pub struct OpenCodeProbeResult {
    pub providers: HashMap<String, bool>,
    pub model_slugs: Vec<String>,
    pub provider_probe_success: bool,
    pub model_probe_success: bool,
    pub error: Option<String>,
}

pub fn decompose_slug(slug: &str) -> Option<DecomposedSlug> {
    let parts: Vec<&str> = slug.split('/').collect();
    match parts.as_slice() {
        [oc_provider, model_part] if !oc_provider.is_empty() && !model_part.is_empty() => {
            Some(DecomposedSlug {
                oc_provider: (*oc_provider).to_string(),
                upstream_provider: None,
                model_part: (*model_part).to_string(),
                full_slug: slug.to_string(),
            })
        }
        [oc_provider, upstream_provider, model_part]
            if !oc_provider.is_empty()
                && !upstream_provider.is_empty()
                && !model_part.is_empty() =>
        {
            Some(DecomposedSlug {
                oc_provider: (*oc_provider).to_string(),
                upstream_provider: Some((*upstream_provider).to_string()),
                model_part: (*model_part).to_string(),
                full_slug: slug.to_string(),
            })
        }
        _ => None,
    }
}

pub fn normalize_model_id(id: &str) -> String {
    id.to_lowercase().replace('.', "-")
}

pub fn model_id_matches(mars_id: &str, oc_model: &str) -> bool {
    normalize_model_id(mars_id) == normalize_model_id(oc_model)
}

pub fn provider_matches(mars_provider: &str, oc_segment: &str) -> bool {
    mars_provider.eq_ignore_ascii_case(oc_segment)
}

/// Classify availability for a model through a specific harness.
/// `probe_result` is None until Phase 2 wires in OpenCode probing.
pub fn classify_for_harness(
    harness: &str,
    provider: &str,
    model_id: &str,
    installed: &HashSet<String>,
    _probe_result: Option<&OpenCodeProbeResult>,
) -> Option<(AvailabilityStatus, AvailabilitySource, Option<RunnablePath>)> {
    let harness = harness.to_ascii_lowercase();
    if !installed.contains(&harness) {
        return Some((
            AvailabilityStatus::Unavailable,
            AvailabilitySource::NoHarness,
            None,
        ));
    }

    let direct_match = match harness.as_str() {
        "claude" => provider_matches(provider, "anthropic"),
        "codex" => provider_matches(provider, "openai"),
        "gemini" => provider_matches(provider, "google"),
        "opencode" => return None,
        _ => false,
    };

    if direct_match {
        Some((
            AvailabilityStatus::Runnable,
            AvailabilitySource::HarnessInstalled,
            Some(RunnablePath {
                harness,
                mars_provider: provider.to_string(),
                harness_model_id: model_id.to_string(),
            }),
        ))
    } else {
        Some((
            AvailabilityStatus::Unavailable,
            AvailabilitySource::NoHarness,
            None,
        ))
    }
}

pub fn classify_model(
    model_id: &str,
    provider: &str,
    installed: &HashSet<String>,
    probe_result: Option<&OpenCodeProbeResult>,
    offline: bool,
) -> ModelAvailability {
    if offline {
        return ModelAvailability {
            status: AvailabilityStatus::Unknown,
            source: AvailabilitySource::Offline,
            runnable_paths: Vec::new(),
        };
    }

    let mut statuses = Vec::new();
    let mut runnable_paths = Vec::new();
    let harnesses = ["claude", "codex", "gemini", "opencode"];

    for harness in harnesses {
        let Some((status, source, path)) =
            classify_for_harness(harness, provider, model_id, installed, probe_result)
        else {
            statuses.push((
                AvailabilityStatus::Unknown,
                AvailabilitySource::OpenCodeProbeUnknown,
            ));
            continue;
        };
        if let Some(path) = path {
            runnable_paths.push(path);
        }
        statuses.push((status, source));
    }

    if !runnable_paths.is_empty() {
        return ModelAvailability {
            status: AvailabilityStatus::Runnable,
            source: AvailabilitySource::HarnessInstalled,
            runnable_paths,
        };
    }

    if statuses
        .iter()
        .any(|(status, _)| *status == AvailabilityStatus::Unknown)
    {
        return ModelAvailability {
            status: AvailabilityStatus::Unknown,
            source: statuses
                .iter()
                .find_map(|(status, source)| {
                    (*status == AvailabilityStatus::Unknown).then(|| source.clone())
                })
                .unwrap_or(AvailabilitySource::OpenCodeProbeUnknown),
            runnable_paths,
        };
    }

    ModelAvailability {
        status: AvailabilityStatus::Unavailable,
        source: AvailabilitySource::NoHarness,
        runnable_paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn test_decompose_slug_two_segments() {
        let slug = decompose_slug("openai/gpt-5.4").unwrap();
        assert_eq!(slug.oc_provider, "openai");
        assert_eq!(slug.upstream_provider, None);
        assert_eq!(slug.model_part, "gpt-5.4");
        assert_eq!(slug.full_slug, "openai/gpt-5.4");
    }

    #[test]
    fn test_decompose_slug_three_segments() {
        let slug = decompose_slug("openrouter/anthropic/claude-opus-4.7").unwrap();
        assert_eq!(slug.oc_provider, "openrouter");
        assert_eq!(slug.upstream_provider.as_deref(), Some("anthropic"));
        assert_eq!(slug.model_part, "claude-opus-4.7");
        assert_eq!(slug.full_slug, "openrouter/anthropic/claude-opus-4.7");
    }

    #[test]
    fn test_decompose_slug_invalid() {
        assert!(decompose_slug("gpt-5").is_none());
        assert!(decompose_slug("openai/").is_none());
        assert!(decompose_slug("a/b/c/d").is_none());
    }

    #[test]
    fn test_normalize_model_id() {
        assert_eq!(normalize_model_id("Claude-Opus-4.7"), "claude-opus-4-7");
    }

    #[test]
    fn test_model_id_matches() {
        assert!(model_id_matches("claude-opus-4-7", "Claude-Opus-4.7"));
        assert!(!model_id_matches("claude-opus-4-7", "claude-sonnet-4-7"));
    }

    #[test]
    fn test_provider_matches() {
        assert!(provider_matches("Anthropic", "anthropic"));
        assert!(!provider_matches("Anthropic", "openai"));
    }

    #[test]
    fn test_classify_claude_anthropic() {
        let result = classify_for_harness(
            "claude",
            "Anthropic",
            "claude-opus-4-7",
            &installed(&["claude"]),
            None,
        )
        .unwrap();
        assert_eq!(result.0, AvailabilityStatus::Runnable);
        assert_eq!(result.1, AvailabilitySource::HarnessInstalled);
        assert_eq!(
            result.2.unwrap().harness_model_id,
            "claude-opus-4-7".to_string()
        );
    }

    #[test]
    fn test_classify_codex_openai() {
        let result =
            classify_for_harness("codex", "OpenAI", "gpt-5.4", &installed(&["codex"]), None)
                .unwrap();
        assert_eq!(result.0, AvailabilityStatus::Runnable);
        assert_eq!(result.1, AvailabilitySource::HarnessInstalled);
    }

    #[test]
    fn test_classify_gemini_google() {
        let result = classify_for_harness(
            "gemini",
            "Google",
            "gemini-2.5-pro",
            &installed(&["gemini"]),
            None,
        )
        .unwrap();
        assert_eq!(result.0, AvailabilityStatus::Runnable);
        assert_eq!(result.1, AvailabilitySource::HarnessInstalled);
    }

    #[test]
    fn test_classify_no_harness() {
        let result = classify_for_harness(
            "claude",
            "Anthropic",
            "claude-opus-4-7",
            &installed(&[]),
            None,
        )
        .unwrap();
        assert_eq!(result.0, AvailabilityStatus::Unavailable);
        assert_eq!(result.1, AvailabilitySource::NoHarness);
        assert!(result.2.is_none());
    }

    #[test]
    fn test_classify_multi_harness_any_runnable() {
        let result = classify_model(
            "claude-opus-4-7",
            "Anthropic",
            &installed(&["claude", "codex"]),
            None,
            false,
        );
        assert_eq!(result.status, AvailabilityStatus::Runnable);
        assert_eq!(result.source, AvailabilitySource::HarnessInstalled);
        assert_eq!(result.runnable_paths.len(), 1);
        assert_eq!(result.runnable_paths[0].harness, "claude");
    }

    #[test]
    fn test_classify_multi_harness_all_unavailable() {
        let result = classify_model("custom-model", "Unknown", &installed(&[]), None, false);
        assert_eq!(result.status, AvailabilityStatus::Unavailable);
        assert_eq!(result.source, AvailabilitySource::NoHarness);
        assert!(result.runnable_paths.is_empty());
    }

    #[test]
    fn test_classify_offline_mode() {
        let result = classify_model("gpt-5.4", "OpenAI", &installed(&["codex"]), None, true);
        assert_eq!(result.status, AvailabilityStatus::Unknown);
        assert_eq!(result.source, AvailabilitySource::Offline);
        assert!(result.runnable_paths.is_empty());
    }
}
