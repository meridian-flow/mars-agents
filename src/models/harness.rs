use std::collections::HashSet;

const HARNESS_BINARIES: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("opencode", "opencode"),
    ("gemini", "gemini"),
];

pub fn detect_installed_harnesses() -> HashSet<String> {
    HARNESS_BINARIES
        .iter()
        .filter(|(_, binary)| harness_binary_exists(binary))
        .map(|(name, _)| name.to_string())
        .collect()
}

fn harness_binary_exists(binary: &str) -> bool {
    if which::which(binary).is_ok() {
        return true;
    }

    #[cfg(windows)]
    {
        ["exe", "cmd", "bat"]
            .iter()
            .any(|ext| which::which(format!("{binary}.{ext}")).is_ok())
    }

    #[cfg(not(windows))]
    {
        false
    }
}

const PROVIDER_HARNESS_PREFERENCES: &[(&str, &[&str])] = &[
    ("anthropic", &["claude", "opencode", "gemini"]),
    ("openai", &["codex", "opencode"]),
    ("google", &["gemini", "opencode"]),
    ("meta", &["opencode"]),
    ("mistral", &["opencode"]),
    ("deepseek", &["opencode"]),
    ("cohere", &["opencode"]),
];

pub fn resolve_harness_for_provider(provider: &str, installed: &HashSet<String>) -> Option<String> {
    let provider_lower = provider.to_lowercase();
    PROVIDER_HARNESS_PREFERENCES
        .iter()
        .find(|(p, _)| *p == provider_lower)
        .and_then(|(_, prefs)| {
            prefs
                .iter()
                .find(|h| installed.contains(**h))
                .map(|h| h.to_string())
        })
}

pub fn harness_candidates_for_provider(provider: &str) -> Vec<String> {
    let provider_lower = provider.to_lowercase();
    PROVIDER_HARNESS_PREFERENCES
        .iter()
        .find(|(p, _)| *p == provider_lower)
        .map(|(_, prefs)| prefs.iter().map(|h| h.to_string()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_harness_anthropic_with_claude() {
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            resolve_harness_for_provider("anthropic", &installed),
            Some("claude".to_string())
        );
    }

    #[test]
    fn resolve_harness_anthropic_falls_back_to_opencode() {
        let installed: HashSet<String> = ["opencode"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            resolve_harness_for_provider("anthropic", &installed),
            Some("opencode".to_string())
        );
    }

    #[test]
    fn resolve_harness_none_installed() {
        let installed: HashSet<String> = HashSet::new();
        assert_eq!(resolve_harness_for_provider("anthropic", &installed), None);
    }

    #[test]
    fn resolve_harness_unknown_provider() {
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            resolve_harness_for_provider("unknown-provider", &installed),
            None
        );
    }

    #[test]
    fn resolve_harness_case_insensitive_provider() {
        let installed: HashSet<String> = ["claude"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            resolve_harness_for_provider("Anthropic", &installed),
            Some("claude".to_string())
        );
    }

    #[test]
    fn candidates_for_known_provider() {
        let candidates = harness_candidates_for_provider("openai");
        assert_eq!(candidates, vec!["codex", "opencode"]);
    }

    #[test]
    fn candidates_for_unknown_provider() {
        let candidates = harness_candidates_for_provider("unknown");
        assert!(candidates.is_empty());
    }
}
