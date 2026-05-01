//! Collision resolution for config entries.
//!
//! Config entries resolve collisions per target root. A collision in one
//! harness target must not globally prune an item that applies cleanly to a
//! different target.

use std::collections::BTreeMap;

use crate::compiler::hooks::{ParsedHookItem, UniversalEvent};
use crate::compiler::mcp::ParsedMcpItem;
use crate::diagnostic::DiagnosticCollector;

const SELF_SOURCE: &str = "_self";

/// Resolve MCP name collisions for a single target root.
///
/// Precedence: `_self` > dependency; earlier `decl_order` > later; equal
/// declaration order breaks alphabetically by source package name.
pub fn resolve_mcp_collisions_for_target<'a>(
    items: &'a [ParsedMcpItem],
    target_root: &str,
    diag: &mut DiagnosticCollector,
) -> Vec<&'a ParsedMcpItem> {
    let mut groups: BTreeMap<&str, Vec<&ParsedMcpItem>> = BTreeMap::new();
    for item in items
        .iter()
        .filter(|item| mcp_applies_to_target(item, target_root))
    {
        groups.entry(item.name.as_str()).or_default().push(item);
    }

    groups
        .into_values()
        .map(|group| resolve_group(group, target_root, "MCP server", diag))
        .collect()
}

/// Resolve hook collisions for a single target root.
///
/// Hook identity is `(event, name)`, so hooks with the same name on different
/// universal events are distinct.
pub fn resolve_hook_collisions_for_target<'a>(
    items: &'a [ParsedHookItem],
    target_root: &str,
    diag: &mut DiagnosticCollector,
) -> Vec<&'a ParsedHookItem> {
    let mut groups: BTreeMap<(UniversalEvent, &str), Vec<&ParsedHookItem>> = BTreeMap::new();
    for item in items
        .iter()
        .filter(|item| hook_applies_to_target(item, target_root))
    {
        groups
            .entry((item.def.event.clone(), item.def.name.as_str()))
            .or_default()
            .push(item);
    }

    groups
        .into_values()
        .map(|group| resolve_group(group, target_root, "hook", diag))
        .collect()
}

trait CollisionItem {
    fn source_name(&self) -> &str;
    fn decl_order(&self) -> usize;
    fn display_name(&self) -> String;
}

impl CollisionItem for ParsedMcpItem {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn decl_order(&self) -> usize {
        self.decl_order
    }

    fn display_name(&self) -> String {
        self.name.clone()
    }
}

impl CollisionItem for ParsedHookItem {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn decl_order(&self) -> usize {
        self.decl_order
    }

    fn display_name(&self) -> String {
        format!("{}:{}", self.def.event, self.def.name)
    }
}

fn resolve_group<'a, T: CollisionItem>(
    mut group: Vec<&'a T>,
    target_root: &str,
    kind: &str,
    diag: &mut DiagnosticCollector,
) -> &'a T {
    debug_assert!(!group.is_empty());
    if group.len() == 1 {
        return group[0];
    }

    group.sort_by(|a, b| {
        let a_self = a.source_name() == SELF_SOURCE;
        let b_self = b.source_name() == SELF_SOURCE;
        b_self
            .cmp(&a_self)
            .then_with(|| a.decl_order().cmp(&b.decl_order()))
            .then_with(|| a.source_name().cmp(b.source_name()))
    });

    let winner = group[0];
    if winner.source_name() == SELF_SOURCE
        && group
            .iter()
            .skip(1)
            .all(|loser| loser.source_name() != SELF_SOURCE)
    {
        return winner;
    }

    for loser in group.iter().skip(1) {
        diag.warn(
            "config-entry-collision",
            format!(
                "{kind} `{}` collision in target `{target_root}`: `{}` wins over `{}`",
                winner.display_name(),
                winner.source_name(),
                loser.source_name()
            ),
        );
    }
    winner
}

fn mcp_applies_to_target(item: &ParsedMcpItem, target_root: &str) -> bool {
    item.def.targets.is_empty() || item.def.targets.iter().any(|t| t == target_root)
}

fn hook_applies_to_target(item: &ParsedHookItem, target_root: &str) -> bool {
    item.def.targets.is_empty() || item.def.targets.iter().any(|t| t == target_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::hooks::discover_hook_items;
    use crate::compiler::mcp::discover_mcp_items;
    use tempfile::TempDir;

    fn make_mcp(dir: &std::path::Path, name: &str, command: &str, targets: Option<&str>) {
        let server_dir = dir.join("mcp").join(name);
        std::fs::create_dir_all(&server_dir).unwrap();
        let targets = targets
            .map(|target| format!("\ntargets = [\"{target}\"]"))
            .unwrap_or_default();
        std::fs::write(
            server_dir.join("mcp.toml"),
            format!("command = \"{command}\"{targets}\n"),
        )
        .unwrap();
    }

    fn make_hook(dir: &std::path::Path, name: &str, event: &str) {
        let hook_dir = dir.join("hooks").join(name);
        std::fs::create_dir_all(&hook_dir).unwrap();
        std::fs::write(
            hook_dir.join("hook.toml"),
            format!(
                r#"
name = "{name}"
event = "{event}"
[action]
kind = "script"
path = "./run.sh"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn self_wins_over_dependency_silently() {
        let local = TempDir::new().unwrap();
        let dep = TempDir::new().unwrap();
        make_mcp(local.path(), "context7", "local-cmd", None);
        make_mcp(dep.path(), "context7", "dep-cmd", None);

        let mut items = discover_mcp_items(local.path(), "_self", 0).unwrap();
        items.extend(discover_mcp_items(dep.path(), "dep", 1).unwrap());
        let mut diag = DiagnosticCollector::new();

        let resolved = resolve_mcp_collisions_for_target(&items, ".claude", &mut diag);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_name, "_self");
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn same_scope_local_mcp_collision_warns() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        make_mcp(first.path(), "context7", "first-cmd", None);
        make_mcp(second.path(), "context7", "second-cmd", None);

        let mut items = discover_mcp_items(second.path(), "_self", 2).unwrap();
        items.extend(discover_mcp_items(first.path(), "_self", 1).unwrap());
        let mut diag = DiagnosticCollector::new();

        let resolved = resolve_mcp_collisions_for_target(&items, ".claude", &mut diag);
        let diagnostics = diag.drain();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_name, "_self");
        assert_eq!(resolved[0].decl_order, 1);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("_self"));
    }

    #[test]
    fn earlier_dependency_wins_with_warning() {
        let early = TempDir::new().unwrap();
        let late = TempDir::new().unwrap();
        make_mcp(early.path(), "context7", "early-cmd", None);
        make_mcp(late.path(), "context7", "late-cmd", None);

        let mut items = discover_mcp_items(late.path(), "late", 2).unwrap();
        items.extend(discover_mcp_items(early.path(), "early", 1).unwrap());
        let mut diag = DiagnosticCollector::new();

        let resolved = resolve_mcp_collisions_for_target(&items, ".claude", &mut diag);
        let diagnostics = diag.drain();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_name, "early");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("early"));
        assert!(diagnostics[0].message.contains("late"));
    }

    #[test]
    fn same_scope_tiebreaks_alphabetically_with_warning() {
        let alpha = TempDir::new().unwrap();
        let zed = TempDir::new().unwrap();
        make_mcp(alpha.path(), "context7", "alpha-cmd", None);
        make_mcp(zed.path(), "context7", "zed-cmd", None);

        let mut items = discover_mcp_items(zed.path(), "zed", 1).unwrap();
        items.extend(discover_mcp_items(alpha.path(), "alpha", 1).unwrap());
        let mut diag = DiagnosticCollector::new();

        let resolved = resolve_mcp_collisions_for_target(&items, ".claude", &mut diag);
        let diagnostics = diag.drain();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_name, "alpha");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("alpha"));
        assert!(diagnostics[0].message.contains("zed"));
    }

    #[test]
    fn mcp_resolution_is_per_target() {
        let claude = TempDir::new().unwrap();
        let codex = TempDir::new().unwrap();
        make_mcp(claude.path(), "context7", "claude-cmd", Some(".claude"));
        make_mcp(codex.path(), "context7", "codex-cmd", Some(".codex"));

        let mut items = discover_mcp_items(claude.path(), "claude", 1).unwrap();
        items.extend(discover_mcp_items(codex.path(), "codex", 2).unwrap());
        let mut diag = DiagnosticCollector::new();

        let claude_resolved = resolve_mcp_collisions_for_target(&items, ".claude", &mut diag);
        let codex_resolved = resolve_mcp_collisions_for_target(&items, ".codex", &mut diag);

        assert_eq!(claude_resolved.len(), 1);
        assert_eq!(claude_resolved[0].source_name, "claude");
        assert_eq!(codex_resolved.len(), 1);
        assert_eq!(codex_resolved[0].source_name, "codex");
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn hook_identity_includes_event_and_name() {
        let pre = TempDir::new().unwrap();
        let post = TempDir::new().unwrap();
        make_hook(pre.path(), "audit", "tool.pre");
        make_hook(post.path(), "audit", "tool.post");

        let mut items = discover_hook_items(pre.path(), "pre-source", 1, 1).unwrap();
        items.extend(discover_hook_items(post.path(), "post-source", 1, 2).unwrap());
        let mut diag = DiagnosticCollector::new();

        let resolved = resolve_hook_collisions_for_target(&items, ".claude", &mut diag);

        assert_eq!(resolved.len(), 2);
        assert!(diag.drain().is_empty());
    }
}
