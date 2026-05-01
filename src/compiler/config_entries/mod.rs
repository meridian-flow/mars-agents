//! Config-entry compiler lane for MCP servers and hooks.
//!
//! This module owns discovery, filtering, lowering, and target-adapter writes
//! for package-defined MCP servers and hooks.

pub mod resolve;
pub mod stale;

use std::collections::{HashMap, VecDeque};

use crate::diagnostic::DiagnosticCollector;
use crate::sync::AppliedState;
use crate::types::{MarsContext, SourceName};

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
pub(crate) fn compile_config_entries(
    ctx: &MarsContext,
    applied: &AppliedState,
    dry_run: bool,
    diag: &mut DiagnosticCollector,
) {
    use crate::compiler::config_entries::resolve::{
        resolve_hook_collisions_for_target, resolve_mcp_collisions_for_target,
    };
    use crate::compiler::hooks::{discover_hook_items, order_hooks, translate_hooks_for_target};
    use crate::compiler::mcp::{TargetMcpEntry, check_env_refs, discover_mcp_items};
    use crate::target::{ConfigEntry, HookEntry, McpServerEntry, TargetRegistry};

    let graph = &applied.planned.targeted.resolved.graph;
    let effective = &applied.planned.targeted.resolved.loaded.effective;
    let target_roots: Vec<String> = effective.settings.managed_targets();

    // Compute package depths via BFS from direct deps (depth 1; local = 0).
    let depths = compute_depths(graph);
    // Compute declaration-order precedence from mars.toml insertion order.
    let decl_orders = compute_decl_orders(graph, &effective.dependencies);

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
    for source_name in &graph.order {
        let Some(node) = graph.nodes.get(source_name) else {
            continue;
        };
        let package_root = &node.rooted_ref.package_root;
        let decl_order = decl_orders
            .get(source_name)
            .copied()
            .unwrap_or(effective.dependencies.len() + graph.order.len() + 1);

        match discover_mcp_items(package_root, source_name.as_str(), decl_order) {
            Ok(items) => all_mcp.extend(items),
            Err(e) => {
                diag.warn(
                    "mcp-discover",
                    format!("failed to scan MCP items in `{source_name}`: {e}"),
                );
            }
        }

        let depth = depths.get(source_name).copied().unwrap_or(1);
        match discover_hook_items(package_root, source_name.as_str(), depth, decl_order) {
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
        let mcp_entries: Vec<ConfigEntry> =
            resolve_mcp_collisions_for_target(&all_mcp, target_root, diag)
                .into_iter()
                .map(TargetMcpEntry::from_parsed)
                .map(|e| {
                    ConfigEntry::McpServer(McpServerEntry {
                        name: e.name,
                        command: e.command,
                        args: e.args,
                        env: e.env.into_iter().collect(),
                    })
                })
                .collect();

        // Resolve and translate hooks for this target, filtering dropped ones.
        let target_hooks: Vec<_> =
            resolve_hook_collisions_for_target(&all_hooks, target_root, diag)
                .into_iter()
                .cloned()
                .collect();
        let translated_hooks = translate_hooks_for_target(order_hooks(target_hooks), target_root);

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
                        let resolved = th.hook
                            .item
                            .package_root
                            .join("hooks")
                            .join(&th.hook.item.def.name)
                            .join(path);
                        // Safety check: resolved path must stay within the package root.
                        if resolved.strip_prefix(&th.hook.item.package_root).is_err() {
                            diag.warn(
                                "hook-path-escape",
                                format!(
                                    "hook `{}`: script path `{path}` escapes package root — skipped",
                                    th.hook.item.def.name
                                ),
                            );
                            return None;
                        }
                        resolved.to_string_lossy().to_string()
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

        // Emit target-specific pre-write diagnostics (runs even on dry runs).
        adapter.emit_pre_write_diagnostics(&entries, diag);

        if !dry_run {
            if let Err(e) = adapter.write_config_entries(&entries, &target_dir) {
                diag.warn(
                    "config-entry-write",
                    format!("failed to write config entries to `{target_root}`: {e}"),
                );
            }
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

/// Compute declaration-order precedence for dependency config entries.
///
/// Direct dependencies use the insertion order from `effective.dependencies`.
/// Transitive dependencies inherit the minimum declaration order of any direct
/// sponsor that reaches them.
fn compute_decl_orders(
    graph: &crate::resolve::ResolvedGraph,
    dependencies: &indexmap::IndexMap<SourceName, crate::config::EffectiveDependency>,
) -> HashMap<SourceName, usize> {
    let mut orders: HashMap<SourceName, usize> = HashMap::new();
    let mut queue: VecDeque<SourceName> = VecDeque::new();

    for (idx, source_name) in dependencies.keys().enumerate() {
        if graph.nodes.contains_key(source_name) {
            orders.insert(source_name.clone(), idx + 1);
            queue.push_back(source_name.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        let current_order = orders[&current];
        let Some(node) = graph.nodes.get(&current) else {
            continue;
        };

        for dep in &node.deps {
            if !graph.nodes.contains_key(dep) {
                continue;
            }
            match orders.get_mut(dep) {
                Some(existing) if current_order < *existing => {
                    *existing = current_order;
                    queue.push_back(dep.clone());
                }
                Some(_) => {}
                None => {
                    orders.insert(dep.clone(), current_order);
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    orders
}
