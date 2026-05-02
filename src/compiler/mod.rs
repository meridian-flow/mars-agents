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
/// Skill frontmatter compiler lane: universal schema parsing and native lowering.
pub mod skills;
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
    SyncReport, SyncRequest,
    apply::{ActionOutcome, ActionTaken},
    apply_plan, build_target, check_frozen_gate, create_plan, finalize, sync_targets,
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
    let agent_surface_policy = agent_surface_policy(
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
    let mars_dir = ctx.project_root.join(".mars");
    reconcile_native_agent_surfaces(
        agent_surface_policy,
        &ctx.project_root,
        &mars_dir,
        &applied.applied.outcomes,
        request.options.dry_run,
        diag,
    );
    if matches!(agent_surface_policy, AgentSurfacePolicy::EmitAll) {
        dual_surface_compile(&ctx.project_root, &mars_dir, request.options.dry_run, diag);
    }

    // Phase 5.1 / 5.2 / 5.3: MCP and hooks config-entry compilation.
    // Discovers MCP server and hook items from all packages, validates env refs,
    // detects collisions, and writes per-target config entries via adapters.
    // Diagnostics run always; file writes are gated on !dry_run.
    let config_entry_records =
        config_entries::compile_config_entries(ctx, &applied, request.options.dry_run, diag);

    // Phase 6: copy from canonical store to managed target directories.
    let mut synced = sync_targets(ctx, applied, request, agent_surface_policy, diag);
    synced.config_entries = config_entry_records;

    // Phase 7: write lock file, build report.
    finalize(ctx, synced, request, diag)
}

/// Describes what happens to agent artifacts on target surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSurfacePolicy {
    /// Emit lowered native agents and copy canonical agents to managed targets.
    EmitAll,
    /// Suppress all agent artifacts on target surfaces.
    SuppressAll,
}

pub fn agent_surface_policy(
    agent_emission: Option<&AgentEmission>,
    meridian_managed: bool,
) -> AgentSurfacePolicy {
    match agent_emission.unwrap_or(&AgentEmission::Auto) {
        AgentEmission::Always => AgentSurfacePolicy::EmitAll,
        AgentEmission::Never => AgentSurfacePolicy::SuppressAll,
        AgentEmission::Auto if meridian_managed => AgentSurfacePolicy::SuppressAll,
        AgentEmission::Auto => AgentSurfacePolicy::EmitAll,
    }
}

/// Convert agent outcomes into removals so target sync can remain a pure
/// materializer with no knowledge of managed-mode policy.
pub fn suppress_agent_outcomes(outcomes: &[ActionOutcome]) -> Vec<ActionOutcome> {
    outcomes
        .iter()
        .cloned()
        .map(|mut outcome| {
            if outcome.item_id.kind == crate::lock::ItemKind::Agent {
                outcome.action = ActionTaken::Removed;
            }
            outcome
        })
        .collect()
}

/// Reconcile native harness agent artifacts written outside target sync.
///
/// Under `SuppressAll`, removes lowered artifacts for all harness-bound agents
/// still present in `.mars/agents/`. Under `EmitAll`, removes only artifacts
/// for agents removed from the canonical store. Removed agents can no longer be
/// inspected for their previous `harness:`, so removal checks every native
/// harness filename shape.
fn reconcile_native_agent_surfaces(
    policy: AgentSurfacePolicy,
    project_root: &Path,
    mars_dir: &Path,
    outcomes: &[crate::sync::apply::ActionOutcome],
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
    use crate::lock::ItemKind;

    if matches!(policy, AgentSurfacePolicy::SuppressAll) {
        remove_current_native_agent_surfaces(project_root, mars_dir, dry_run, diag);
    }

    for outcome in outcomes {
        if outcome.item_id.kind != ItemKind::Agent
            || !matches!(outcome.action, ActionTaken::Removed)
        {
            continue;
        }

        let agent_name = outcome.dest_path.item_name(ItemKind::Agent);
        remove_native_agent_shapes(project_root, &agent_name, dry_run, diag);
    }
}

fn remove_current_native_agent_surfaces(
    project_root: &Path,
    mars_dir: &Path,
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
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

        let agent_name = profile.name.as_deref().unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });
        remove_native_agent_shapes(project_root, agent_name, dry_run, diag);
    }
}

fn remove_native_agent_shapes(
    project_root: &Path,
    agent_name: &str,
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
    use crate::compiler::agents::HarnessKind;

    for harness in HarnessKind::all() {
        let target = harness.target_dir();
        for extension in ["md", "toml"] {
            let native_path = project_root
                .join(target)
                .join("agents")
                .join(format!("{agent_name}.{extension}"));
            if !native_path.exists() && native_path.symlink_metadata().is_err() {
                continue;
            }
            if dry_run {
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

#[cfg(test)]
mod skill_surface_tests {
    use super::*;
    use crate::compiler::agents::HarnessKind;
    use crate::diagnostic::DiagnosticCollector;
    use crate::lock::{ItemId, ItemKind};
    use crate::sync::apply::{ActionOutcome, ActionTaken};
    use crate::types::{DestPath, ItemName};
    use tempfile::TempDir;

    #[test]
    fn native_agent_emission_defaults_to_standalone_auto() {
        assert_eq!(
            agent_surface_policy(None, false),
            AgentSurfacePolicy::EmitAll
        );
    }

    #[test]
    fn native_agent_emission_auto_suppresses_meridian_managed() {
        assert_eq!(
            agent_surface_policy(Some(&AgentEmission::Auto), true),
            AgentSurfacePolicy::SuppressAll
        );
    }

    #[test]
    fn native_agent_emission_always_ignores_meridian_managed() {
        assert_eq!(
            agent_surface_policy(Some(&AgentEmission::Always), true),
            AgentSurfacePolicy::EmitAll
        );
    }

    #[test]
    fn native_agent_emission_never_suppresses_standalone() {
        assert_eq!(
            agent_surface_policy(Some(&AgentEmission::Never), false),
            AgentSurfacePolicy::SuppressAll
        );
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
    fn reconcile_emit_all_removes_native_shapes_for_removed_agents() {
        let dir = TempDir::new().unwrap();
        for harness in HarnessKind::all() {
            let agents_dir = dir.path().join(harness.target_dir()).join("agents");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(agents_dir.join("coder.md"), "# Old\n").unwrap();
            std::fs::write(agents_dir.join("coder.toml"), "old = true\n").unwrap();
        }

        let mut diag = DiagnosticCollector::new();
        reconcile_native_agent_surfaces(
            AgentSurfacePolicy::EmitAll,
            dir.path(),
            &dir.path().join(".mars"),
            &[agent_outcome("coder", ActionTaken::Removed)],
            false,
            &mut diag,
        );

        for harness in HarnessKind::all() {
            assert!(
                !dir.path()
                    .join(harness.target_dir())
                    .join("agents/coder.md")
                    .exists()
            );
            assert!(
                !dir.path()
                    .join(harness.target_dir())
                    .join("agents/coder.toml")
                    .exists()
            );
        }
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn reconcile_suppress_all_removes_native_shapes_for_current_agents() {
        let dir = TempDir::new().unwrap();

        // Set up a canonical agent in .mars/agents/
        let mars_agents = dir.path().join(".mars").join("agents");
        std::fs::create_dir_all(&mars_agents).unwrap();
        std::fs::write(
            mars_agents.join("coder.md"),
            "---\nname: coder\n---\n# Coder\n",
        )
        .unwrap();

        // Set up native artifacts that should be cleaned
        for target in [".claude", ".codex", ".opencode"] {
            let agents_dir = dir.path().join(target).join("agents");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(agents_dir.join("coder.md"), "# Native\n").unwrap();
        }

        let mut diag = DiagnosticCollector::new();
        reconcile_native_agent_surfaces(
            AgentSurfacePolicy::SuppressAll,
            dir.path(),
            &dir.path().join(".mars"),
            // No removed outcomes — suppression removes current agents regardless
            &[agent_outcome("coder", ActionTaken::Installed)],
            false,
            &mut diag,
        );

        for target in [".claude", ".codex", ".opencode"] {
            assert!(
                !dir.path().join(target).join("agents/coder.md").exists(),
                "native agent should be removed under SuppressAll for target {target}"
            );
        }
    }

    #[test]
    fn reconcile_emit_all_preserves_non_removed_agents() {
        let dir = TempDir::new().unwrap();

        // Set up native artifacts for a non-removed agent
        let agents_dir = dir.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("coder.md"), "# Native\n").unwrap();

        let mut diag = DiagnosticCollector::new();
        reconcile_native_agent_surfaces(
            AgentSurfacePolicy::EmitAll,
            dir.path(),
            &dir.path().join(".mars"),
            // Agent is Installed (not Removed) — should be preserved
            &[agent_outcome("coder", ActionTaken::Installed)],
            false,
            &mut diag,
        );

        // Native artifact should still exist
        assert!(dir.path().join(".claude/agents/coder.md").exists());
    }
}
