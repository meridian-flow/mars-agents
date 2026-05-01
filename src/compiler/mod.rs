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

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use crate::compiler::context::CompileContext;
use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::model::ReaderIr;
use crate::sync::{
    AppliedState, SyncReport, SyncRequest, apply_plan, build_target, check_frozen_gate,
    create_plan, finalize, sync_targets,
};
use crate::types::{MarsContext, SourceName};

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

    // Phase 5.1 / 5.2 / 5.3: MCP and hooks config-entry compilation.
    // Discovers MCP server and hook items from all packages, validates env refs,
    // detects collisions, and writes per-target config entries via adapters.
    if !request.options.dry_run {
        compile_config_entries(ctx, &applied, diag);
    }

    // Phase 6: copy from canonical store to managed target directories.
    let synced = sync_targets(ctx, applied, request, diag);

    // Phase 7: write lock file, build report.
    finalize(ctx, synced, request, diag)
}

/// Phase 5 config-entry compilation: MCP servers and hooks.
///
/// For each package in the resolved graph:
/// 1. Discover MCP items from `mcp/<name>/mcp.toml`
/// 2. Discover hook items from `hooks/<name>/hook.toml`
///
/// Then:
/// 3. Run env-ref preflight (warn on missing vars)
/// 4. Detect per-target-root MCP name collisions
/// 5. Order hooks deterministically
/// 6. For each target root, lower items and write via adapter `write_config_entries()`
///
/// All errors are non-fatal — emitted as diagnostics and compilation continues.
fn compile_config_entries(
    ctx: &MarsContext,
    applied: &AppliedState,
    diag: &mut DiagnosticCollector,
) {
    use crate::compiler::hooks::{discover_hook_items, order_hooks, translate_hooks_for_target};
    use crate::compiler::mcp::{
        check_env_refs, detect_mcp_collisions, discover_mcp_items, lower_for_target,
    };
    use crate::target::cursor::CursorAdapter;
    use crate::target::{ConfigEntry, HookEntry, McpServerEntry, TargetRegistry};

    let graph = &applied.planned.targeted.resolved.graph;
    let effective = &applied.planned.targeted.resolved.loaded.effective;
    let target_roots: Vec<String> = effective.settings.managed_targets();
    let target_root_strs: Vec<&str> = target_roots.iter().map(|s| s.as_str()).collect();

    // Compute package depths via BFS from direct deps (depth 1; local = 0).
    let depths = compute_depths(graph);

    // Collect all MCP and hook items across all packages.
    let mut all_mcp: Vec<crate::compiler::mcp::ParsedMcpItem> = Vec::new();
    let mut all_hooks: Vec<crate::compiler::hooks::ParsedHookItem> = Vec::new();

    // Local package (depth 0, decl_order 0).
    let local_mcp = match discover_mcp_items(&ctx.project_root, "_self", 0) {
        Ok(items) => items,
        Err(e) => {
            diag.warn(
                "mcp-discover",
                format!("failed to scan local MCP items: {e}"),
            );
            Vec::new()
        }
    };
    all_mcp.extend(local_mcp);

    let local_hooks = match discover_hook_items(&ctx.project_root, "_self", 0, 0) {
        Ok(items) => items,
        Err(e) => {
            diag.warn(
                "hook-discover",
                format!("failed to scan local hook items: {e}"),
            );
            Vec::new()
        }
    };
    all_hooks.extend(local_hooks);

    // Dependency packages.
    for (decl_order, source_name) in graph.order.iter().enumerate() {
        let Some(node) = graph.nodes.get(source_name) else {
            continue;
        };
        let depth = depths.get(source_name).copied().unwrap_or(1);
        let package_root = &node.rooted_ref.package_root;

        match discover_mcp_items(package_root, source_name.as_str(), depth) {
            Ok(items) => all_mcp.extend(items),
            Err(e) => {
                diag.warn(
                    "mcp-discover",
                    format!("failed to scan MCP items in `{source_name}`: {e}"),
                );
            }
        }

        match discover_hook_items(package_root, source_name.as_str(), depth, decl_order + 1) {
            Ok(items) => all_hooks.extend(items),
            Err(e) => {
                diag.warn(
                    "hook-discover",
                    format!("failed to scan hook items in `{source_name}`: {e}"),
                );
            }
        }
    }

    // HIGH-3: Filter out hooks and MCP items from dependency packages where visibility is Local.
    {
        use crate::compiler::visibility::{can_cross_package_boundary, resolve_visibility};
        use crate::lock::ItemKind;

        all_mcp.retain(|item| {
            // Local package items always pass.
            if item.source_name == "_self" {
                return true;
            }
            // Dependency item — check visibility.
            let explicit = match item.def.visibility.as_str() {
                "exported" => Some(true),
                "local" => Some(false),
                _ => None, // treat unknown as default (local)
            };
            let vis = resolve_visibility(ItemKind::McpServer, &item.name, explicit);
            if !can_cross_package_boundary(&vis) {
                return false;
            }
            // Emit warning for explicitly exported effectful items.
            true
        });

        all_hooks.retain(|item| {
            // Local package items always pass.
            if item.source_name == "_self" {
                return true;
            }
            // Dependency item — check visibility.
            let explicit = match item.def.visibility.as_str() {
                "exported" => Some(true),
                "local" => Some(false),
                _ => None,
            };
            let vis = resolve_visibility(ItemKind::Hook, &item.def.name, explicit);
            can_cross_package_boundary(&vis)
        });
    }

    // Env ref preflight (non-strict by default).
    if let Err(e) = check_env_refs(&all_mcp, false, diag) {
        diag.warn("mcp-env", format!("MCP env check failed: {e}"));
    }

    // Collision detection — hard error (MEDIUM-1).
    if let Err(e) = detect_mcp_collisions(&all_mcp, &target_root_strs) {
        diag.warn("mcp-collision", format!("{e}"));
        return; // Hard error: stop config-entry compilation.
    }

    // Order hooks deterministically.
    let ordered_hooks = order_hooks(all_hooks);

    // Get the target registry.
    let registry = TargetRegistry::new();

    // For each target root, lower and write config entries.
    for target_root in &target_roots {
        let target_dir = ctx.project_root.join(target_root);
        if !target_dir.is_dir() {
            // Target directory doesn't exist — skip (not configured/enabled).
            continue;
        }

        // Lower MCP items for this target.
        let mcp_entries: Vec<ConfigEntry> = lower_for_target(&all_mcp, target_root)
            .into_iter()
            .map(|e| {
                ConfigEntry::McpServer(McpServerEntry {
                    name: e.name,
                    command: e.command,
                    args: e.args,
                    env: e.env.into_iter().collect(),
                })
            })
            .collect();

        // Translate hooks for this target, filtering dropped ones.
        let translated_hooks = translate_hooks_for_target(ordered_hooks.clone(), target_root);

        // Emit lossiness diagnostics for dropped and approximate hooks.
        for th in &translated_hooks {
            match th.lossiness {
                crate::compiler::hooks::LossinessKind::Dropped => {
                    diag.warn(
                        "hook-dropped",
                        format!(
                            "hook `{}` (event `{}`) dropped for target `{target_root}` — \
                             no native hook support",
                            th.hook.item.def.name, th.hook.item.def.event
                        ),
                    );
                }
                crate::compiler::hooks::LossinessKind::Approximate => {
                    diag.info(
                        "hook-approximate",
                        format!(
                            "hook `{}` (event `{}`) approximately mapped for target \
                             `{target_root}` — semantics may differ slightly",
                            th.hook.item.def.name, th.hook.item.def.event
                        ),
                    );
                }
                crate::compiler::hooks::LossinessKind::Exact => {}
            }
        }

        // Build hook config entries (only non-dropped ones).
        let hook_entries: Vec<ConfigEntry> = translated_hooks
            .into_iter()
            .filter_map(|th| {
                let native_event = th.native_event?;
                let script_path = match &th.hook.item.def.action {
                    crate::compiler::hooks::HookAction::Script { path } => {
                        // Resolve script path relative to the package root the hook came from.
                        th.hook
                            .item
                            .package_root
                            .join("hooks")
                            .join(&th.hook.item.def.name)
                            .join(path)
                            .to_string_lossy()
                            .to_string()
                    }
                };
                Some(ConfigEntry::Hook(HookEntry {
                    name: th.hook.item.def.name.clone(),
                    event: th.hook.item.def.event.to_string(),
                    native_event,
                    script_path,
                    order: th.hook.item.def.order,
                }))
            })
            .collect();

        // Combine all entries.
        let mut entries = mcp_entries;
        entries.extend(hook_entries);

        if entries.is_empty() {
            continue;
        }

        // Write via the target adapter (if one is registered).
        let Some(adapter) = registry.get(target_root) else {
            // No adapter registered — skip.
            continue;
        };

        // Emit Cursor-specific hook lossiness diagnostics.
        if target_root == ".cursor" {
            CursorAdapter::emit_hook_lossiness_diagnostics(&entries, diag);
        }

        if let Err(e) = adapter.write_config_entries(&entries, &target_dir) {
            diag.warn(
                "config-entry-write",
                format!("failed to write config entries to `{target_root}`: {e}"),
            );
        }
    }
}

/// Compute package depth for hook ordering.
///
/// Direct dependencies of the consumer project have depth 1.
/// Their transitive dependencies have depth 2, etc.
/// Packages at the leaf of the graph (no dependencies themselves) have the highest depth.
///
/// Returns a map from SourceName → depth.
fn compute_depths(graph: &crate::resolve::ResolvedGraph) -> HashMap<SourceName, usize> {
    // Build reverse adjacency: for each package, which packages it's a dep of.
    // We want BFS from "packages with no inbound edges" — those that no other package depends on.
    // Actually we want the opposite: BFS from packages that nobody else depends on (the leafs),
    // assigning them the highest depth, and the "root" deps get the lowest depth.

    // Simpler: compute depth as the length of the longest path from the package to a leaf.
    // But that's expensive. Use the topological order instead.

    // Approach: packages in graph.order are in topological order (deps first, dependents last).
    // The packages that appear FIRST in topological order (no predecessors) are depth 1.
    // The packages they depend on are depth 2+.
    // Wait, that's also complex.

    // Simplest correct approach: BFS from "packages nobody else depends on" (they are direct deps).
    let mut in_degree: HashMap<SourceName, usize> = HashMap::new();
    for name in graph.nodes.keys() {
        in_degree.insert(name.clone(), 0);
    }
    for node in graph.nodes.values() {
        for dep in &node.deps {
            if graph.nodes.contains_key(dep) {
                *in_degree.entry(dep.clone()).or_insert(0) += 1;
            }
        }
    }

    // Direct dependencies of the consumer project have in_degree 0.
    let mut depths: HashMap<SourceName, usize> = HashMap::new();
    let mut queue: VecDeque<SourceName> = VecDeque::new();

    for (name, degree) in &in_degree {
        if *degree == 0 {
            depths.insert(name.clone(), 1);
            queue.push_back(name.clone());
        }
    }

    // BFS to assign depths to transitives.
    while let Some(current) = queue.pop_front() {
        let current_depth = depths[&current];
        if let Some(node) = graph.nodes.get(&current) {
            for dep in &node.deps {
                if graph.nodes.contains_key(dep) {
                    depths
                        .entry(dep.clone())
                        .and_modify(|d| *d = (*d).max(current_depth + 1))
                        .or_insert_with(|| {
                            queue.push_back(dep.clone());
                            current_depth + 1
                        });
                }
            }
        }
    }

    depths
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
fn dual_surface_compile(project_root: &Path, mars_dir: &Path, diag: &mut DiagnosticCollector) {
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
