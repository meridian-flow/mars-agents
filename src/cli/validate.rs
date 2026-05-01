//! `mars validate` — dry-run compiler that reports diagnostics without writing.
//!
//! Runs the same reader/compiler pipeline as `mars sync --diff` but stops
//! before any writes and reports diagnostics with structured output options.
//!
//! Output modes:
//! - Normal: print diagnostics, exit 0 if clean, exit 1 if errors present.
//! - `--strict`: escalate warnings to errors (missing env vars, etc.).
//! - `--json`: emit diagnostics as structured JSON for tooling.

use serde::Serialize;

use crate::cli::MarsContext;
use crate::diagnostic::{Diagnostic, DiagnosticCategory, DiagnosticLevel};
use crate::error::MarsError;
use crate::sync::{ResolutionMode, SyncOptions, SyncRequest};

/// Arguments for `mars validate`.
#[derive(Debug, clap::Args)]
pub struct ValidateArgs {
    /// Escalate warnings to errors (e.g., missing env vars become errors).
    #[arg(long)]
    pub strict: bool,
}

/// JSON output envelope for `mars validate --json`.
#[derive(Debug, Serialize)]
pub struct ValidateReport {
    /// Whether the validate run is clean (no errors after applying strictness rules).
    pub clean: bool,
    /// All diagnostics collected during the dry-run pipeline.
    pub diagnostics: Vec<ValidateDiagnostic>,
    /// Number of errors (after strictness escalation).
    pub error_count: usize,
    /// Number of warnings (after strictness escalation, pre-escalated warnings
    /// that became errors are NOT counted here).
    pub warning_count: usize,
}

/// A single diagnostic in JSON output.
#[derive(Debug, Serialize)]
pub struct ValidateDiagnostic {
    pub level: &'static str,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

impl ValidateDiagnostic {
    fn from_diagnostic(d: &Diagnostic, strict: bool) -> Self {
        let level = effective_level(d.level, strict);
        ValidateDiagnostic {
            level: level_str(level),
            code: d.code,
            message: d.message.clone(),
            context: d.context.clone(),
            category: d.category.map(category_str),
        }
    }
}

/// Run `mars validate`.
pub fn run(args: &ValidateArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: None,
        options: SyncOptions {
            force: false,
            dry_run: true,
            frozen: false,
            no_refresh_models: false,
        },
    };

    // Load config to get min_mars_version for compatibility preflight.
    // This is a lightweight read that doesn't acquire the sync lock.
    let min_required: Option<String> = crate::config::load(&ctx.project_root)
        .ok()
        .and_then(|cfg| cfg.settings.min_mars_version);

    // Run the pipeline in dry-run mode (no writes).
    // ValidationWarnings are included in report.diagnostics by finalize().
    let report = crate::sync::execute(ctx, &request)?;

    // Run compatibility preflight against the binary version and project setting.
    let binary_version = env!("CARGO_PKG_VERSION");
    let mut all_diagnostics: Vec<Diagnostic> = report.diagnostics.clone();
    if let Some(compat_diag) =
        crate::diagnostic::compatibility_preflight(binary_version, min_required.as_deref())
    {
        // Compatibility errors are prepended so they appear first.
        all_diagnostics.insert(0, compat_diag);
    }

    // Compute effective counts (--strict escalates warnings to errors).
    let error_count = all_diagnostics
        .iter()
        .filter(|d| effective_level(d.level, args.strict) == DiagnosticLevel::Error)
        .count();
    let warning_count = all_diagnostics
        .iter()
        .filter(|d| effective_level(d.level, args.strict) == DiagnosticLevel::Warning)
        .count();
    let clean = error_count == 0;

    if json {
        let validate_diags: Vec<ValidateDiagnostic> = all_diagnostics
            .iter()
            .map(|d| ValidateDiagnostic::from_diagnostic(d, args.strict))
            .collect();
        let validate_report = ValidateReport {
            clean,
            diagnostics: validate_diags,
            error_count,
            warning_count,
        };
        super::output::print_json(&validate_report);
    } else {
        print_text_report(&all_diagnostics, args.strict);
        println!();
        if clean {
            super::output::print_success("validate: clean");
        } else {
            super::output::print_error(&format!(
                "validate: {error_count} error(s){}",
                if warning_count > 0 {
                    format!(", {warning_count} warning(s)")
                } else {
                    String::new()
                }
            ));
        }
    }

    if clean { Ok(0) } else { Ok(1) }
}

fn print_text_report(diagnostics: &[Diagnostic], strict: bool) {
    for diag in diagnostics {
        let level = effective_level(diag.level, strict);
        let prefix = level_str(level);
        if let Some(ctx) = &diag.context {
            eprintln!("  {prefix}[{}]: {} ({})", diag.code, diag.message, ctx);
        } else {
            eprintln!("  {prefix}[{}]: {}", diag.code, diag.message);
        }
    }
}

/// When `--strict` is active, escalate Warning to Error.
fn effective_level(level: DiagnosticLevel, strict: bool) -> DiagnosticLevel {
    if strict && level == DiagnosticLevel::Warning {
        DiagnosticLevel::Error
    } else {
        level
    }
}

fn level_str(level: DiagnosticLevel) -> &'static str {
    match level {
        DiagnosticLevel::Error => "error",
        DiagnosticLevel::Warning => "warning",
        DiagnosticLevel::Info => "info",
    }
}

fn category_str(cat: DiagnosticCategory) -> &'static str {
    match cat {
        DiagnosticCategory::Compatibility => "compatibility",
        DiagnosticCategory::Lossiness => "lossiness",
        DiagnosticCategory::Validation => "validation",
        DiagnosticCategory::Config => "config",
    }
}

#[cfg(test)]
fn validation_warning_to_diagnostic(vw: &crate::validate::ValidationWarning) -> Diagnostic {
    use crate::validate::ValidationWarning;
    match vw {
        ValidationWarning::MissingSkill {
            agent,
            skill_name,
            suggestion,
        } => {
            let message = if let Some(s) = suggestion {
                format!(
                    "agent `{}` references missing skill `{skill_name}` (did you mean `{s}`?)",
                    agent.name
                )
            } else {
                format!(
                    "agent `{}` references missing skill `{skill_name}`",
                    agent.name
                )
            };
            Diagnostic {
                level: DiagnosticLevel::Warning,
                code: "missing-skill",
                message,
                context: None,
                category: Some(DiagnosticCategory::Validation),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticLevel;

    fn make_diag(level: DiagnosticLevel) -> Diagnostic {
        Diagnostic {
            level,
            code: "test",
            message: "test message".to_string(),
            context: None,
            category: None,
        }
    }

    #[test]
    fn strict_mode_escalates_warning_to_error() {
        let diag = make_diag(DiagnosticLevel::Warning);
        assert_eq!(effective_level(diag.level, true), DiagnosticLevel::Error);
    }

    #[test]
    fn strict_mode_leaves_error_as_error() {
        let diag = make_diag(DiagnosticLevel::Error);
        assert_eq!(effective_level(diag.level, true), DiagnosticLevel::Error);
    }

    #[test]
    fn non_strict_leaves_warning_as_warning() {
        let diag = make_diag(DiagnosticLevel::Warning);
        assert_eq!(effective_level(diag.level, false), DiagnosticLevel::Warning);
    }

    #[test]
    fn strict_mode_leaves_info_as_info() {
        let diag = make_diag(DiagnosticLevel::Info);
        assert_eq!(effective_level(diag.level, true), DiagnosticLevel::Info);
    }

    #[test]
    fn validate_diag_from_diagnostic_maps_category() {
        let diag = Diagnostic {
            level: DiagnosticLevel::Warning,
            code: "compat-version",
            message: "test".to_string(),
            context: None,
            category: Some(DiagnosticCategory::Compatibility),
        };
        let vd = ValidateDiagnostic::from_diagnostic(&diag, false);
        assert_eq!(vd.level, "warning");
        assert_eq!(vd.category, Some("compatibility"));
    }

    #[test]
    fn validate_diag_strict_escalation_in_json() {
        let diag = Diagnostic {
            level: DiagnosticLevel::Warning,
            code: "missing-skill",
            message: "test".to_string(),
            context: None,
            category: Some(DiagnosticCategory::Validation),
        };
        let vd = ValidateDiagnostic::from_diagnostic(&diag, true);
        assert_eq!(
            vd.level, "error",
            "warning should be escalated in strict mode"
        );
    }

    #[test]
    fn validation_warning_missing_skill_no_suggestion() {
        use crate::lock::{ItemId, ItemKind};
        use crate::types::ItemName;
        let vw = crate::validate::ValidationWarning::MissingSkill {
            agent: ItemId {
                kind: ItemKind::Agent,
                name: ItemName::from("coder".to_string()),
            },
            skill_name: "planning".to_string(),
            suggestion: None,
        };
        let diag = validation_warning_to_diagnostic(&vw);
        assert_eq!(diag.level, DiagnosticLevel::Warning);
        assert!(diag.message.contains("coder"));
        assert!(diag.message.contains("planning"));
        assert_eq!(diag.category, Some(DiagnosticCategory::Validation));
    }

    #[test]
    fn validation_warning_missing_skill_with_suggestion() {
        use crate::lock::{ItemId, ItemKind};
        use crate::types::ItemName;
        let vw = crate::validate::ValidationWarning::MissingSkill {
            agent: ItemId {
                kind: ItemKind::Agent,
                name: ItemName::from("coder".to_string()),
            },
            skill_name: "plan".to_string(),
            suggestion: Some("planning".to_string()),
        };
        let diag = validation_warning_to_diagnostic(&vw);
        assert!(diag.message.contains("did you mean `planning`"));
    }
}
