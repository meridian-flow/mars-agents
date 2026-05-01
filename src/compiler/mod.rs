/// Compiler stage — target building, diff, plan, apply, lock finalization.
///
/// `compile()` is the second half of the sync pipeline. It consumes a
/// [`crate::model::ReaderIr`] (all source-level facts) and produces a
/// [`crate::sync::SyncReport`] by assigning dest paths, computing diffs,
/// writing files, syncing managed targets, and persisting the lock.
/// Agent-profile schema parser, routing prepass, and per-target lowering.
pub mod agents;
pub mod context;
/// Hook compiler lane: discovery, event validation, ordering, lossiness classification.
pub mod hooks;
/// MCP server compiler lane: discovery, env-ref validation, collision detection.
pub mod mcp;
/// Skill placement, output planning, and compile-time overlap/visibility checks.
pub mod skills;
/// Visibility propagation rules for passive vs effectful items (D1/D10).
pub mod visibility;

use std::path::Path;

use crate::compiler::context::CompileContext;
use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::model::ReaderIr;
use crate::sync::{
    SyncReport, SyncRequest, apply_plan, build_target, check_frozen_gate, create_plan, finalize,
    sync_targets,
};
use crate::types::MarsContext;

/// Run the compiler stage: `ReaderIr` → target state → plan → apply → `SyncReport`.
pub fn compile(
    ctx: &MarsContext,
    ir: ReaderIr,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<SyncReport, MarsError> {
    // Phase 2+ scaffolding: translation context wired here so the seam exists
    // when the pipeline is extended to produce per-target output records
    // (lossiness classification, hook script selection). Unused today.
    let _compile_ctx = CompileContext::new();

    // Phase 3: assign dest paths, handle collisions, rewrite frontmatter refs.
    let targeted = build_target(ctx, ir.resolved, ir.local_items, request, diag)?;

    // Phase 4: diff + plan.
    let planned = create_plan(ctx, targeted, request, diag)?;

    // Frozen gate: no pending changes allowed.
    if request.options.frozen {
        check_frozen_gate(&planned)?;
    }

    // Phase 5: persist config mutations, apply plan to canonical store.
    let applied = apply_plan(ctx, planned, request)?;

    // Phase 3.2 / 3.3: Dual-surface compilation — after apply writes agents to
    // .mars/agents/, compile harness-bound agents to their native target directories.
    if !request.options.dry_run {
        let mars_dir = ctx.project_root.join(".mars");
        dual_surface_compile(&ctx.project_root, &mars_dir, diag);
    }

    // Phase 6: copy from canonical store to managed target directories.
    let synced = sync_targets(ctx, applied, request, diag);

    // Phase 7: write lock file, build report.
    finalize(ctx, synced, request, diag)
}

/// Dual-surface compilation: scan `.mars/agents/` for harness-bound agents and
/// emit native artifacts into the project root.
///
/// For each `*.md` file in `.mars/agents/`:
/// 1. Parse the agent profile frontmatter.
/// 2. Emit lossiness warnings for dropped fields.
/// 3. If `harness:` is set, lower to native format and write to
///    `<project_root>/<harness_dir>/agents/<name>.<ext>`.
///
/// Errors are non-fatal — they are emitted as diagnostics and the sync continues.
/// This preserves the "target sync is non-fatal" principle (D9).
fn dual_surface_compile(
    project_root: &Path,
    mars_dir: &Path,
    diag: &mut DiagnosticCollector,
) {
    use crate::compiler::agents::HarnessKind;
    use crate::compiler::agents::lower::lower_for_harness;
    use crate::compiler::agents::parse_agent_content;

    let agents_dir = mars_dir.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        // .mars/agents/ doesn't exist yet (e.g., first dry-run or empty project).
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension() else { continue };
        if ext != "md" {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                diag.warn(
                    "dual-surface-read",
                    format!("could not read {}: {e}", path.display()),
                );
                continue;
            }
        };

        let mut agent_diags = Vec::new();
        let (profile, fm) = match parse_agent_content(&content, &mut agent_diags) {
            Ok(r) => r,
            Err(e) => {
                diag.warn(
                    "dual-surface-parse",
                    format!("could not parse {}: {e}", path.display()),
                );
                continue;
            }
        };

        // Emit agent-level diagnostics (validation errors, legacy fields)
        let agent_name = profile
            .name
            .as_deref()
            .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown"));
        for d in &agent_diags {
            if d.is_error() {
                diag.warn(
                    "agent-schema-error",
                    format!("agent `{agent_name}`: {}", d.message()),
                );
            } else {
                diag.warn(
                    "agent-schema-warning",
                    format!("agent `{agent_name}`: {}", d.message()),
                );
            }
        }

        // If no harness:, this is a universal agent — only .agents/ output, done.
        let Some(harness) = &profile.harness else {
            continue;
        };

        // Lower to native format.
        let body = fm.body().to_string();
        let lowered = lower_for_harness(harness, &profile, &fm, &body);

        // Emit lossiness diagnostics.
        for lf in &lowered.lossy_fields {
            use crate::compiler::agents::lower::Lossiness;
            match &lf.classification {
                Lossiness::Dropped | Lossiness::MeridianOnly => {
                    diag.warn(
                        "agent-field-dropped",
                        format!(
                            "agent `{agent_name}`: field `{}` dropped in {} native artifact",
                            lf.field, lf.target
                        ),
                    );
                }
                Lossiness::Approximate { note } => {
                    diag.warn(
                        "agent-field-approximate",
                        format!(
                            "agent `{agent_name}`: field `{}` approximately mapped in {} ({note})",
                            lf.field, lf.target
                        ),
                    );
                }
                Lossiness::Exact => {}
            }
        }

        // Determine native artifact path.
        let harness_dir = project_root.join(harness.target_dir());
        let native_agents_dir = harness_dir.join("agents");
        let file_name = match harness {
            HarnessKind::Codex => format!("{agent_name}.toml"),
            _ => format!("{agent_name}.md"),
        };
        let native_path = native_agents_dir.join(&file_name);

        // Write native artifact (atomic tmp+rename).
        if let Err(e) = std::fs::create_dir_all(&native_agents_dir) {
            diag.warn(
                "dual-surface-mkdir",
                format!("could not create {}: {e}", native_agents_dir.display()),
            );
            continue;
        }

        if let Err(e) = crate::fs::atomic_write(&native_path, &lowered.bytes) {
            diag.warn(
                "dual-surface-write",
                format!("could not write {}: {e}", native_path.display()),
            );
        }
    }
}
