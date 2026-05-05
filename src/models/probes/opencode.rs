use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

/// Result of probing OpenCode's runtime availability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenCodeProbeResult {
    /// Provider availability: OpenCode provider ID -> has credentials.
    pub providers: HashMap<String, bool>,
    /// Full model slug list, e.g. `openai/gpt-5.4`.
    pub model_slugs: Vec<String>,
    /// Whether the provider probe succeeded.
    pub provider_probe_success: bool,
    /// Whether the model list probe succeeded.
    pub model_probe_success: bool,
    /// Redacted error message if either probe failed.
    pub error: Option<String>,
}

const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 5;

/// Probe OpenCode with the configured timeout.
pub fn probe() -> OpenCodeProbeResult {
    probe_with_timeout(probe_timeout())
}

/// Probe OpenCode with a specific timeout.
pub fn probe_with_timeout(timeout: Duration) -> OpenCodeProbeResult {
    let deadline = Instant::now() + timeout;
    let mut result = OpenCodeProbeResult::default();

    match run_command("opencode", &["providers", "list"], timeout) {
        Ok(stdout) => {
            result.providers = parse_providers_output(&stdout);
            result.provider_probe_success = true;
        }
        Err(error) => {
            result.error = Some(format!("provider probe failed: {error}"));
            result.provider_probe_success = false;
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        result.model_probe_success = false;
        if result.error.is_none() {
            result.error = Some("timeout before model probe".to_string());
        }
        return result;
    }

    match run_command("opencode", &["models"], remaining) {
        Ok(stdout) => {
            result.model_slugs = parse_models_output(&stdout);
            result.model_probe_success = true;
        }
        Err(error) => {
            result.model_probe_success = false;
            if result.error.is_none() {
                result.error = Some(format!("model probe failed: {error}"));
            }
        }
    }

    result
}

fn probe_timeout() -> Duration {
    std::env::var("MARS_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS))
}

fn run_command(cmd: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let program = resolve_command(cmd);
    let mut child = Command::new(&program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn failed: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout capture unavailable".to_string())?;
    let stdout_reader = thread::spawn(move || {
        let mut stdout = stdout;
        let mut output = Vec::new();
        stdout
            .read_to_end(&mut output)
            .map(|_| output)
            .map_err(|error| format!("stdout read failed: {error}"))
    });

    match child
        .wait_timeout(timeout)
        .map_err(|error| format!("wait failed: {error}"))?
    {
        Some(status) if status.success() => {
            let stdout = stdout_reader
                .join()
                .map_err(|_| "stdout reader panicked".to_string())??;
            String::from_utf8(stdout).map_err(|error| format!("invalid utf8: {error}"))
        }
        Some(status) => {
            let _ = stdout_reader.join();
            Err(format!("exit code {}", status.code().unwrap_or(-1)))
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            Err("timeout".to_string())
        }
    }
}

fn resolve_command(cmd: &str) -> PathBuf {
    if let Ok(path) = which::which(cmd) {
        return path;
    }

    #[cfg(windows)]
    {
        for ext in ["exe", "cmd", "bat"] {
            if let Ok(path) = which::which(format!("{cmd}.{ext}")) {
                return path;
            }
        }
    }

    PathBuf::from(cmd)
}

fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Parse `opencode providers list` output.
///
/// SECURITY: The raw input may reference credential paths. Do not log, store,
/// or include it in diagnostics.
fn parse_providers_output(stdout: &str) -> HashMap<String, bool> {
    let mut providers = HashMap::new();

    for line in stdout.lines() {
        let clean = strip_ansi(line.trim());
        if let Some(rest) = clean.strip_prefix('●').or_else(|| clean.strip_prefix('*')) {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                providers.insert(parts[0].to_lowercase(), true);
            }
        }
    }

    providers
}

/// Parse `opencode models` output into slug list.
fn parse_models_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| strip_ansi(line.trim()))
        .filter(|line| !line.is_empty() && line.contains('/'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_basic() {
        let input = "\x1b[32mGreen\x1b[0m";
        assert_eq!(strip_ansi(input), "Green");
    }

    #[test]
    fn test_strip_ansi_no_escapes() {
        assert_eq!(strip_ansi("Plain text"), "Plain text");
    }

    #[test]
    fn test_parse_providers_bullet() {
        let output = r#"┌  Credentials [path redacted]
│
●  OpenAI oauth
│
●  Google api
│
●  OpenRouter api
│
└  3 credentials"#;

        let providers = parse_providers_output(output);

        assert!(providers.get("openai").copied().unwrap_or(false));
        assert!(providers.get("google").copied().unwrap_or(false));
        assert!(providers.get("openrouter").copied().unwrap_or(false));
        assert!(!providers.contains_key("credentials"));
    }

    #[test]
    fn test_parse_providers_empty() {
        assert!(parse_providers_output("").is_empty());
    }

    #[test]
    fn test_parse_models_basic() {
        let output = r#"opencode/big-pickle
google/gemini-2.5-pro
openai/gpt-5.4
openrouter/anthropic/claude-opus-4.7"#;

        let slugs = parse_models_output(output);

        assert_eq!(slugs.len(), 4);
        assert!(slugs.contains(&"openai/gpt-5.4".to_string()));
        assert!(slugs.contains(&"openrouter/anthropic/claude-opus-4.7".to_string()));
    }

    #[test]
    fn test_parse_models_filters_invalid() {
        let slugs = parse_models_output("some-invalid-line\nopenai/gpt-5.4\n\n");
        assert_eq!(slugs, vec!["openai/gpt-5.4"]);
    }

    #[test]
    fn test_parse_models_strips_ansi() {
        let slugs = parse_models_output("\x1b[32mopenai/gpt-5.4\x1b[0m");
        assert_eq!(slugs, vec!["openai/gpt-5.4"]);
    }

    #[test]
    fn test_probe_result_round_trip() {
        let result = OpenCodeProbeResult {
            providers: HashMap::from([("openai".to_string(), true)]),
            model_slugs: vec!["openai/gpt-5.4".to_string()],
            provider_probe_success: true,
            model_probe_success: true,
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: OpenCodeProbeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.providers.get("openai"), Some(&true));
        assert_eq!(back.model_slugs, result.model_slugs);
        assert!(back.provider_probe_success);
        assert!(back.model_probe_success);
        assert_eq!(back.error, None);
    }
}
