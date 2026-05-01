use serde::Serialize;

/// A diagnostic message from library code.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    /// Machine-readable code, e.g. "shadow-collision", "manifest-path-dep".
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
    /// Optional context (source name, item path, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Diagnostic category for tooling and structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<DiagnosticCategory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticLevel {
    Error,
    Warning,
    Info,
}

/// Broad category for a diagnostic — used in structured output and validation gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticCategory {
    /// Compatibility and version requirement issues.
    Compatibility,
    /// Lossiness during lowering to a target (dropped/approximate fields).
    Lossiness,
    /// Schema validation and structural checks.
    Validation,
    /// Configuration file issues.
    Config,
}

/// Collects diagnostics during pipeline execution.
pub struct DiagnosticCollector {
    diagnostics: Vec<Diagnostic>,
}

impl DiagnosticCollector {
    pub fn new() -> Self {
        Self {
            diagnostics: Vec::new(),
        }
    }

    pub fn error(&mut self, code: &'static str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Error,
            code,
            message: message.into(),
            context: None,
            category: None,
        });
    }

    pub fn error_with_category(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        category: DiagnosticCategory,
    ) {
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Error,
            code,
            message: message.into(),
            context: None,
            category: Some(category),
        });
    }

    pub fn warn(&mut self, code: &'static str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Warning,
            code,
            message: message.into(),
            context: None,
            category: None,
        });
    }

    pub fn info(&mut self, code: &'static str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Info,
            code,
            message: message.into(),
            context: None,
            category: None,
        });
    }

    pub fn warn_with_context(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        context: impl Into<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            level: DiagnosticLevel::Warning,
            code,
            message: message.into(),
            context: Some(context.into()),
            category: None,
        });
    }

    pub fn extend(&mut self, diagnostics: Vec<Diagnostic>) {
        self.diagnostics.extend(diagnostics);
    }

    pub fn drain(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Returns true if any Error-level diagnostic has been collected.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.level == DiagnosticLevel::Error)
    }
}

impl Default for DiagnosticCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self.level {
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Info => "info",
        };
        write!(f, "{prefix}: {}", self.message)
    }
}

/// Compatibility preflight: check whether the current binary version satisfies a
/// `min_mars_version` requirement declared by the project's mars.toml.
///
/// Returns `None` if compatible (or no requirement is declared).
/// Returns `Some(Diagnostic)` with `Error` level if the binary is too old.
///
/// Rule:
/// - `min_required` is `None` → always compatible (old package without requirement)
/// - `binary_version >= min_required` → compatible
/// - `binary_version < min_required` → error: binary too old
pub fn compatibility_preflight(
    binary_version: &str,
    min_required: Option<&str>,
) -> Option<Diagnostic> {
    let min = min_required?;

    // Parse as semver. If either fails to parse, accept and emit a warning instead.
    let bin_ver = parse_semver(binary_version);
    let req_ver = parse_semver(min);

    match (bin_ver, req_ver) {
        (Some(bin), Some(req)) => {
            if bin < req {
                Some(Diagnostic {
                    level: DiagnosticLevel::Error,
                    code: "compat-version",
                    message: format!(
                        "this project requires mars >= {min} but the installed binary is {binary_version}; \
                         upgrade with: cargo install mars-agents"
                    ),
                    context: None,
                    category: Some(DiagnosticCategory::Compatibility),
                })
            } else {
                None
            }
        }
        _ => {
            // Unparseable version strings — warn and continue (forward compat: new package,
            // unknown version scheme → don't hard-block the consumer).
            Some(Diagnostic {
                level: DiagnosticLevel::Warning,
                code: "compat-version-parse",
                message: format!(
                    "could not compare mars version `{binary_version}` against requirement `{min}`; \
                     proceeding with defaults"
                ),
                context: None,
                category: Some(DiagnosticCategory::Compatibility),
            })
        }
    }
}

/// Minimal semver parser: returns `(major, minor, patch)` tuple from "X.Y.Z" or "vX.Y.Z".
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim_start_matches('v');
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = parts[1].parse::<u64>().ok()?;
    // Allow patch to have pre-release suffix like "1-beta.1"
    let patch_str = parts[2].split('-').next().unwrap_or(parts[2]);
    let patch = patch_str.parse::<u64>().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_requirement_always_compatible() {
        let diag = compatibility_preflight("0.5.0", None);
        assert!(diag.is_none());
    }

    #[test]
    fn binary_meets_requirement() {
        let diag = compatibility_preflight("1.2.0", Some("1.0.0"));
        assert!(diag.is_none());
    }

    #[test]
    fn binary_exactly_meets_requirement() {
        let diag = compatibility_preflight("1.0.0", Some("1.0.0"));
        assert!(diag.is_none());
    }

    #[test]
    fn binary_too_old_produces_error() {
        let diag = compatibility_preflight("0.5.0", Some("1.0.0")).unwrap();
        assert_eq!(diag.level, DiagnosticLevel::Error);
        assert_eq!(diag.code, "compat-version");
        assert_eq!(diag.category, Some(DiagnosticCategory::Compatibility));
        assert!(diag.message.contains("0.5.0"));
        assert!(diag.message.contains("1.0.0"));
    }

    #[test]
    fn binary_v_prefix_handled() {
        let diag = compatibility_preflight("v1.2.0", Some("v1.0.0"));
        assert!(diag.is_none());
    }

    #[test]
    fn binary_v_prefix_too_old() {
        let diag = compatibility_preflight("v0.9.0", Some("v1.0.0")).unwrap();
        assert_eq!(diag.level, DiagnosticLevel::Error);
    }

    #[test]
    fn unparseable_version_produces_warning_not_error() {
        let diag = compatibility_preflight("dev", Some("1.0.0")).unwrap();
        assert_eq!(diag.level, DiagnosticLevel::Warning);
        assert_eq!(diag.code, "compat-version-parse");
    }

    #[test]
    fn unparseable_requirement_produces_warning() {
        let diag = compatibility_preflight("1.0.0", Some("latest")).unwrap();
        assert_eq!(diag.level, DiagnosticLevel::Warning);
    }

    #[test]
    fn collector_has_errors_detects_error_level() {
        let mut coll = DiagnosticCollector::new();
        assert!(!coll.has_errors());
        coll.warn("w", "a warning");
        assert!(!coll.has_errors());
        coll.error("e", "an error");
        assert!(coll.has_errors());
    }

    #[test]
    fn collector_error_with_category() {
        let mut coll = DiagnosticCollector::new();
        coll.error_with_category(
            "compat-version",
            "too old",
            DiagnosticCategory::Compatibility,
        );
        let diags = coll.drain();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].level, DiagnosticLevel::Error);
        assert_eq!(diags[0].category, Some(DiagnosticCategory::Compatibility));
    }

    #[test]
    fn display_shows_error_prefix() {
        let d = Diagnostic {
            level: DiagnosticLevel::Error,
            code: "test",
            message: "something broke".to_string(),
            context: None,
            category: None,
        };
        assert_eq!(d.to_string(), "error: something broke");
    }

    #[test]
    fn display_shows_warning_prefix() {
        let d = Diagnostic {
            level: DiagnosticLevel::Warning,
            code: "test",
            message: "heads up".to_string(),
            context: None,
            category: None,
        };
        assert_eq!(d.to_string(), "warning: heads up");
    }

    #[test]
    fn patch_with_prerelease_suffix_parsed() {
        // "1.2.3-beta.1" → (1, 2, 3)
        let v = parse_semver("1.2.3-beta.1").unwrap();
        assert_eq!(v, (1, 2, 3));
    }
}
