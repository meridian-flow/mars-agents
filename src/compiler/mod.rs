/// Compiler stage — target building, diff, plan, apply, lock finalization.
///
/// `compile()` is the second half of the sync pipeline. It consumes a
/// [`crate::model::ReaderIr`] (all source-level facts) and produces a
/// [`crate::sync::SyncReport`] by assigning dest paths, computing diffs,
/// writing files, syncing managed targets, and persisting the lock.
/// Agent-profile schema parser, routing prepass, and per-target lowering.
pub mod agents;
pub mod config_entries;
pub mod context;
/// Hook compiler lane: discovery, event validation, ordering, lossiness classification.
pub mod hooks;
/// MCP server compiler lane: discovery, env-ref validation, collision detection.
pub mod mcp;
/// Skill variant layout validation, indexing, and projection helpers.
pub mod variants;
/// Visibility propagation rules for passive vs effectful items (D1/D10).
pub mod visibility;

use std::path::Path;

use crate::config::AgentEmission;
use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::model::ReaderIr;
use crate::sync::{
    SyncReport, SyncRequest, apply::ActionTaken, apply_plan, build_target, check_frozen_gate,
    create_plan, finalize, sync_targets,
};
use crate::types::MarsContext;

/// Run the compiler stage: `ReaderIr` → target state → plan → apply → `SyncReport`.
pub fn compile(
    ctx: &MarsContext,
    ir: ReaderIr,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<SyncReport, MarsError> {
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
    // Diagnostics run always; file writes are gated on !dry_run.
    {
        let mars_dir = ctx.project_root.join(".mars");
        let emit_native_agents = should_emit_native_agents(
            applied
                .planned
                .targeted
                .resolved
                .loaded
                .config
                .settings
                .agent_emission
                .as_ref(),
            ctx.meridian_managed,
        );
        cleanup_removed_native_agents(
            &ctx.project_root,
            &applied.applied.outcomes,
            request.options.dry_run,
            diag,
        );
        if emit_native_agents {
            dual_surface_compile(&ctx.project_root, &mars_dir, request.options.dry_run, diag);
        } else {
            remove_native_agent_surfaces(
                &ctx.project_root,
                &mars_dir,
                request.options.dry_run,
                diag,
            );
        }
    }

    // Phase 5.1 / 5.2 / 5.3: MCP and hooks config-entry compilation.
    // Discovers MCP server and hook items from all packages, validates env refs,
    // detects collisions, and writes per-target config entries via adapters.
    // Diagnostics run always; file writes are gated on !dry_run.
    let config_entry_records =
        config_entries::compile_config_entries(ctx, &applied, request.options.dry_run, diag);

    // Phase 6: copy from canonical store to managed target directories.
    let mut synced = sync_targets(ctx, applied, request, diag);
    synced.config_entries = config_entry_records;

    // Phase 7: write lock file, build report.
    finalize(ctx, synced, request, diag)
}

/// Remove stale native harness agent artifacts for agents that were removed
/// from the canonical `.mars/agents/` store.
///
/// Removed agents can no longer be inspected for their previous `harness:`
/// value, so cleanup checks every native harness agent filename shape
/// (`*.md` and `*.toml`) under every native agent surface. Missing files are
/// ignored and removal errors are non-fatal diagnostics.
fn cleanup_removed_native_agents(
    project_root: &Path,
    outcomes: &[crate::sync::apply::ActionOutcome],
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
    use crate::lock::ItemKind;

    if dry_run {
        return;
    }

    for outcome in outcomes {
        if outcome.item_id.kind != ItemKind::Agent
            || !matches!(outcome.action, ActionTaken::Removed)
        {
            continue;
        }

        let agent_name = outcome.dest_path.item_name(ItemKind::Agent);
        for target in [".claude", ".codex", ".opencode", ".cursor", ".pi"] {
            for extension in ["md", "toml"] {
                let native_path = project_root
                    .join(target)
                    .join("agents")
                    .join(format!("{agent_name}.{extension}"));
                if !native_path.exists() && native_path.symlink_metadata().is_err() {
                    continue;
                }
                if let Err(e) = crate::reconcile::fs_ops::safe_remove(&native_path) {
                    diag.warn(
                        "native-agent-remove",
                        format!("could not remove {}: {e}", native_path.display()),
                    );
                }
            }
        }
    }
}

/// Remove native harness agent artifacts for harness-bound agents currently in
/// `.mars/agents/` when native agent emission is disabled.
///
/// This keeps sync idempotent when switching from standalone mode (native
/// agents emitted) to Meridian-managed or `agent_emission = "never"` mode
/// (native agents suppressed). It intentionally only touches agents that still
/// exist in the canonical `.mars/` store and declare a harness, avoiding broad
/// deletion of user-created native harness agents.
fn remove_native_agent_surfaces(
    project_root: &Path,
    mars_dir: &Path,
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
    use crate::compiler::agents::HarnessKind;
    use crate::compiler::agents::parse_agent_content;

    let agents_dir = mars_dir.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                diag.warn(
                    "native-agent-remove-read",
                    format!("could not read {}: {e}", path.display()),
                );
                continue;
            }
        };

        let mut agent_diags = Vec::new();
        let (profile, _fm) = match parse_agent_content(&content, &mut agent_diags) {
            Ok(r) => r,
            Err(e) => {
                diag.warn(
                    "native-agent-remove-parse",
                    format!("could not parse {}: {e}", path.display()),
                );
                continue;
            }
        };

        let Some(harness) = &profile.harness else {
            continue;
        };
        let agent_name = profile.name.as_deref().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });
        let file_name = match harness {
            HarnessKind::Codex => format!("{agent_name}.toml"),
            _ => format!("{agent_name}.md"),
        };
        let native_path = project_root
            .join(harness.target_dir())
            .join("agents")
            .join(file_name);

        if dry_run || (!native_path.exists() && native_path.symlink_metadata().is_err()) {
            continue;
        }
        if let Err(e) = crate::reconcile::fs_ops::safe_remove(&native_path) {
            diag.warn(
                "native-agent-remove",
                format!("could not remove {}: {e}", native_path.display()),
            );
        }
    }
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
    dry_run: bool,
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
        let Some(ext) = path.extension() else {
            continue;
        };
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
        let agent_name = profile.name.as_deref().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });
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

        // If no harness:, this is a universal agent — only .mars/ canonical output, done.
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

        // Write native artifact (atomic tmp+rename) — skipped on dry runs.
        if !dry_run {
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
}

fn should_emit_native_agents(
    agent_emission: Option<&AgentEmission>,
    meridian_managed: bool,
) -> bool {
    match agent_emission.unwrap_or(&AgentEmission::Auto) {
        AgentEmission::Always => true,
        AgentEmission::Never => false,
        AgentEmission::Auto => !meridian_managed,
    }
}

#[cfg(test)]
mod skill_surface_tests {
    use super::*;
    use crate::diagnostic::DiagnosticCollector;
    use crate::lock::{ItemId, ItemKind};
    use crate::sync::apply::{ActionOutcome, ActionTaken};
    use crate::types::{DestPath, ItemName};
    use tempfile::TempDir;

    #[test]
    fn native_agent_emission_defaults_to_standalone_auto() {
        assert!(should_emit_native_agents(None, false));
    }

    #[test]
    fn native_agent_emission_auto_suppresses_meridian_managed() {
        assert!(!should_emit_native_agents(Some(&AgentEmission::Auto), true));
    }

    #[test]
    fn native_agent_emission_always_ignores_meridian_managed() {
        assert!(should_emit_native_agents(
            Some(&AgentEmission::Always),
            true
        ));
    }

    #[test]
    fn native_agent_emission_never_suppresses_standalone() {
        assert!(!should_emit_native_agents(
            Some(&AgentEmission::Never),
            false
        ));
    }

    fn agent_outcome(name: &str, action: ActionTaken) -> ActionOutcome {
        ActionOutcome {
            item_id: ItemId {
                kind: ItemKind::Agent,
                name: ItemName::from(name),
            },
            action,
            dest_path: DestPath::from(format!("agents/{name}.md")),
            source_name: "test-source".into(),
            source_checksum: None,
            installed_checksum: None,
        }
    }

    #[test]
    fn cleanup_removed_native_agents_removes_all_native_filename_shapes() {
        let dir = TempDir::new().unwrap();
        for target in [".claude", ".codex", ".opencode", ".cursor", ".pi"] {
            let agents_dir = dir.path().join(target).join("agents");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(agents_dir.join("coder.md"), "# Old\n").unwrap();
            std::fs::write(agents_dir.join("coder.toml"), "old = true\n").unwrap();
        }

        let mut diag = DiagnosticCollector::new();
        cleanup_removed_native_agents(
            dir.path(),
            &[agent_outcome("coder", ActionTaken::Removed)],
            false,
            &mut diag,
        );

        for target in [".claude", ".codex", ".opencode", ".cursor", ".pi"] {
            assert!(!dir.path().join(target).join("agents/coder.md").exists());
            assert!(!dir.path().join(target).join("agents/coder.toml").exists());
        }
        assert!(diag.drain().is_empty());
    }
}
