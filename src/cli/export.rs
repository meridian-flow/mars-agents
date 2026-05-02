//! `mars export` — produce a JSON representation of the compile plan.
//!
//! Runs the same dry-run pipeline as `mars validate` but outputs structured
//! JSON describing the full compile plan: dependencies, items, outputs,
//! and diagnostics. Designed for tooling that needs to inspect what mars
//! would do without executing it.
//!
//! Constraints:
//! - Read-only: no write-path side effects.
//! - No rendered file bodies in output — only metadata.
//! - No host-absolute paths in output except documented opaque command strings
//!   (hook `command` fields, which are absolute by necessity).

use serde::Serialize;

use crate::cli::MarsContext;
use crate::error::MarsError;
use crate::sync::{ResolutionMode, SyncOptions, SyncRequest};

/// JSON schema version for the export envelope.
const SCHEMA_VERSION: u32 = 1;

/// Arguments for `mars export`.
#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    // No extra flags for now — the command always outputs JSON.
    // Future: --target <filter> to restrict to specific target roots.
}

// ── Output types ──────────────────────────────────────────────────────────────

/// Top-level export envelope.
///
/// Schema versioned for forward compatibility. The `status` field indicates
/// whether the compile plan is complete, partial, or failed.
#[derive(Debug, Serialize)]
pub struct ExportEnvelope {
    /// Format version — increment when the JSON shape changes incompatibly.
    pub schema_version: u32,
    /// Overall compile plan status.
    pub status: ExportStatus,
    /// Dependency metadata layer: what the project declares as dependencies.
    pub dependencies: Vec<ExportDependency>,
    /// Item layer: all items in the compile plan.
    pub items: Vec<ExportItem>,
    /// Output layer: per-item output records (dest paths, target roots).
    pub outputs: Vec<ExportOutput>,
    /// Diagnostic layer: all diagnostics from the pipeline.
    pub diagnostics: Vec<ExportDiagnostic>,
}

/// Overall status of the compile plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportStatus {
    /// All items compiled successfully — no conflicts or errors.
    Complete,
    /// Some items have conflicts or warnings that prevent clean install.
    Partial,
    /// The compile pipeline failed entirely (resolver error, I/O error, etc.).
    Failed,
}

/// One declared dependency from mars.toml.
#[derive(Debug, Serialize)]
pub struct ExportDependency {
    /// Logical name of the dependency (key in [dependencies]).
    pub name: String,
    /// Resolved version tag or commit, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Source origin kind: "git", "path", or "registry".
    pub origin: String,
}

/// One item in the compile plan (agent, skill, hook, mcp, etc.).
#[derive(Debug, Serialize)]
pub struct ExportItem {
    /// Item name.
    pub name: String,
    /// Item kind: "agent", "skill", "hook", "mcp-server", "bootstrap-doc".
    pub kind: String,
    /// Source dependency that provides this item.
    pub source: String,
    /// Planned action: "install", "overwrite", "skip", "conflict", "remove".
    pub action: String,
}

/// One output record — where an item lands in the target.
#[derive(Debug, Serialize)]
pub struct ExportOutput {
    /// Item name this output belongs to.
    pub item_name: String,
    /// Item kind.
    pub kind: String,
    /// Destination path within the managed directory (relative, no absolute prefix).
    pub dest_path: String,
    /// Source dependency name.
    pub source: String,
}

/// One diagnostic in export output.
#[derive(Debug, Serialize)]
pub struct ExportDiagnostic {
    pub level: &'static str,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
}

// ── Command implementation ────────────────────────────────────────────────────

/// Run `mars export`.
///
/// Always outputs JSON (the `--json` global flag is accepted but redundant here).
pub fn run(_args: &ExportArgs, ctx: &MarsContext, _json: bool) -> Result<i32, MarsError> {
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

    // Load config for dependency metadata (non-fatal: if missing, no dep metadata).
    let config = crate::config::load(&ctx.project_root).unwrap_or_default();

    // Build the dependency layer from declared dependencies in config.
    let dependencies: Vec<ExportDependency> = config
        .dependencies
        .iter()
        .chain(config.local_dependencies.iter())
        .map(|(name, dep)| ExportDependency {
            name: name.to_string(),
            version: dep.version.clone(),
            origin: infer_origin(dep),
        })
        .collect();

    // Run the pipeline in dry-run mode to get the compile plan.
    let (status, items, outputs, diagnostics) = match crate::sync::execute(ctx, &request) {
        Ok(report) => {
            let has_conflicts = report.has_conflicts();
            let status = if has_conflicts {
                ExportStatus::Partial
            } else {
                ExportStatus::Complete
            };

            let mut items: Vec<ExportItem> = Vec::new();
            let mut outputs: Vec<ExportOutput> = Vec::new();

            for outcome in &report.applied.outcomes {
                let action = action_label(&outcome.action);
                let name = outcome.item_id.name.to_string();
                let kind = kind_label(&outcome.item_id.kind);
                let source = outcome.source_name.to_string();
                let dest_path = outcome.dest_path.to_string();

                items.push(ExportItem {
                    name: name.clone(),
                    kind: kind.clone(),
                    source: source.clone(),
                    action: action.to_string(),
                });
                outputs.push(ExportOutput {
                    item_name: name,
                    kind,
                    dest_path,
                    source,
                });
            }

            // Include pruned (remove) actions.
            for outcome in &report.pruned {
                let name = outcome.item_id.name.to_string();
                let kind = kind_label(&outcome.item_id.kind);
                let source = outcome.source_name.to_string();
                let dest_path = outcome.dest_path.to_string();

                items.push(ExportItem {
                    name: name.clone(),
                    kind: kind.clone(),
                    source: source.clone(),
                    action: "remove".to_string(),
                });
                outputs.push(ExportOutput {
                    item_name: name,
                    kind,
                    dest_path,
                    source,
                });
            }

            let diagnostics = report
                .diagnostics
                .iter()
                .map(export_diagnostic)
                .collect::<Vec<_>>();

            (status, items, outputs, diagnostics)
        }
        Err(err) => {
            // Compile failed entirely — report as failed with the error as a diagnostic.
            let diagnostics = vec![ExportDiagnostic {
                level: "error",
                code: "pipeline-failed",
                message: err.to_string(),
                context: None,
                category: Some("config"),
            }];
            (ExportStatus::Failed, vec![], vec![], diagnostics)
        }
    };

    let envelope = ExportEnvelope {
        schema_version: SCHEMA_VERSION,
        status,
        dependencies,
        items,
        outputs,
        diagnostics,
    };

    super::output::print_json(&envelope);
    Ok(0)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn infer_origin(dep: &crate::config::InstallDep) -> String {
    if dep.url.is_some() {
        "git".to_string()
    } else if dep.path.is_some() {
        "path".to_string()
    } else {
        "registry".to_string()
    }
}

fn action_label(action: &crate::sync::apply::ActionTaken) -> &'static str {
    use crate::sync::apply::ActionTaken;
    match action {
        ActionTaken::Installed => "install",
        ActionTaken::Updated => "overwrite",
        ActionTaken::Merged => "merge",
        ActionTaken::Conflicted => "conflict",
        ActionTaken::Removed => "remove",
        ActionTaken::Skipped => "skip",
        ActionTaken::Kept => "skip",
    }
}

fn kind_label(kind: &crate::lock::ItemKind) -> String {
    use crate::lock::ItemKind;
    match kind {
        ItemKind::Agent => "agent".to_string(),
        ItemKind::Skill => "skill".to_string(),
        ItemKind::Hook => "hook".to_string(),
        ItemKind::McpServer => "mcp-server".to_string(),
        ItemKind::BootstrapDoc => "bootstrap-doc".to_string(),
    }
}

fn export_diagnostic(d: &crate::diagnostic::Diagnostic) -> ExportDiagnostic {
    use crate::diagnostic::{DiagnosticCategory, DiagnosticLevel};
    ExportDiagnostic {
        level: match d.level {
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Info => "info",
        },
        code: d.code,
        message: d.message.clone(),
        context: d.context.clone(),
        category: d.category.map(|c| match c {
            DiagnosticCategory::Compatibility => "compatibility",
            DiagnosticCategory::Lossiness => "lossiness",
            DiagnosticCategory::Validation => "validation",
            DiagnosticCategory::Config => "config",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_nonzero() {
        const { assert!(SCHEMA_VERSION >= 1) };
    }

    #[test]
    fn export_status_serializes_lowercase() {
        let complete = serde_json::to_string(&ExportStatus::Complete).unwrap();
        let partial = serde_json::to_string(&ExportStatus::Partial).unwrap();
        let failed = serde_json::to_string(&ExportStatus::Failed).unwrap();
        assert_eq!(complete, r#""complete""#);
        assert_eq!(partial, r#""partial""#);
        assert_eq!(failed, r#""failed""#);
    }

    #[test]
    fn envelope_includes_schema_version() {
        let env = ExportEnvelope {
            schema_version: 1,
            status: ExportStatus::Complete,
            dependencies: vec![],
            items: vec![],
            outputs: vec![],
            diagnostics: vec![],
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("\"schema_version\":1"),
            "missing schema_version: {json}"
        );
    }

    #[test]
    fn envelope_no_file_bodies() {
        // ExportEnvelope must not have any field that could hold file content.
        // Verified structurally: ExportItem, ExportOutput, ExportDependency
        // have no "content", "body", or "source_content" fields.
        let item = ExportItem {
            name: "coder".to_string(),
            kind: "agent".to_string(),
            source: "meridian-base".to_string(),
            action: "install".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(
            !json.contains("content"),
            "item should not have content field"
        );
        assert!(!json.contains("body"), "item should not have body field");
    }

    #[test]
    fn export_dependency_origin_git() {
        use crate::config::InstallDep;
        use crate::types::SourceUrl;
        let dep = InstallDep {
            url: Some(SourceUrl::from("https://github.com/org/repo")),
            path: None,
            subpath: None,
            version: None,
            filter: Default::default(),
        };
        assert_eq!(infer_origin(&dep), "git");
    }

    #[test]
    fn export_dependency_origin_path() {
        use crate::config::InstallDep;
        let dep = InstallDep {
            url: None,
            path: Some(std::path::PathBuf::from("../local-pkg")),
            subpath: None,
            version: None,
            filter: Default::default(),
        };
        assert_eq!(infer_origin(&dep), "path");
    }

    #[test]
    fn export_diagnostic_maps_levels() {
        use crate::diagnostic::{Diagnostic, DiagnosticLevel};
        let d = Diagnostic {
            level: DiagnosticLevel::Error,
            code: "test",
            message: "msg".to_string(),
            context: None,
            category: None,
        };
        let ed = export_diagnostic(&d);
        assert_eq!(ed.level, "error");
        assert_eq!(ed.code, "test");
        assert_eq!(ed.category, None);
    }
}
