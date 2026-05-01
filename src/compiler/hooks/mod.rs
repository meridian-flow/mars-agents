/// Hook compiler lane.
///
/// Discovers, parses, validates, orders, and translates hook definitions
/// from package trees into per-target config entries.
///
/// V0 scope:
/// - Universal event vocabulary: `session.start`, `session.end`, `tool.pre`, `tool.post`
/// - Non-V0 events are rejected with a hard error
/// - Per-target lossiness classification: exact | approximate | dropped
/// - Deterministic total ordering: depth → dependency order → `order` field → name
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{ConfigError, MarsError};

// ---------------------------------------------------------------------------
// Universal event vocabulary (V0)
// ---------------------------------------------------------------------------

/// V0 universal hook events.
///
/// Any event string not in this list is rejected by the compiler.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UniversalEvent {
    SessionStart,
    SessionEnd,
    ToolPre,
    ToolPost,
}

impl UniversalEvent {
    /// Parse a string into a universal event, rejecting unknown/non-V0 values.
    pub fn parse(s: &str) -> Result<Self, MarsError> {
        match s {
            "session.start" => Ok(Self::SessionStart),
            "session.end" => Ok(Self::SessionEnd),
            "tool.pre" => Ok(Self::ToolPre),
            "tool.post" => Ok(Self::ToolPost),
            other => Err(MarsError::Config(ConfigError::Invalid {
                message: format!(
                    "unknown or unsupported hook event `{other}` — \
                     V0 events are: session.start, session.end, tool.pre, tool.post"
                ),
            })),
        }
    }

    /// The canonical event string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session.start",
            Self::SessionEnd => "session.end",
            Self::ToolPre => "tool.pre",
            Self::ToolPost => "tool.post",
        }
    }
}

impl fmt::Display for UniversalEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

/// The action a hook performs.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum HookAction {
    /// Run a script.
    #[serde(rename = "script")]
    Script {
        /// Path to the script, relative to the hook directory.
        path: String,
    },
}

/// Raw deserialization form of a hook definition.
/// Exists so we can parse the `event` as a plain string before validating.
#[derive(Debug, Deserialize)]
struct RawHookDef {
    name: String,
    event: String,
    #[serde(default = "default_visibility")]
    visibility: String,
    #[serde(default)]
    targets: Vec<String>,
    action: HookAction,
    #[serde(default)]
    order: i32,
}

fn default_visibility() -> String {
    "local".to_string()
}

/// A parsed, validated hook definition.
#[derive(Debug, Clone)]
pub struct HookDef {
    pub name: String,
    pub event: UniversalEvent,
    pub visibility: String,
    pub targets: Vec<String>,
    pub action: HookAction,
    /// Explicit ordering hint (lower = earlier).
    pub order: i32,
}

/// A discovered hook item with provenance.
#[derive(Debug, Clone)]
pub struct ParsedHookItem {
    pub def: HookDef,
    /// Source package name.
    pub source_name: String,
    /// Depth in the dependency graph (0 = root package).
    pub package_depth: usize,
    /// Position of the source in the dependency declaration order.
    /// Used for stable ordering within the same depth.
    pub dep_decl_order: usize,
    /// Absolute path to the package root this hook was discovered in.
    pub package_root: PathBuf,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover hook items from a package root.
///
/// Scans `<package_root>/hooks/<name>/hook.toml` for each subdirectory.
pub fn discover_hook_items(
    package_root: &Path,
    source_name: &str,
    package_depth: usize,
    dep_decl_order: usize,
) -> Result<Vec<ParsedHookItem>, MarsError> {
    let hooks_dir = package_root.join("hooks");
    if !hooks_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&hooks_dir)
        .map_err(MarsError::from)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir_name = entry.file_name();
        let hook_dir_name = dir_name.to_string_lossy();
        if hook_dir_name.starts_with('.') {
            continue;
        }

        let toml_path = entry.path().join("hook.toml");
        if !toml_path.is_file() {
            continue;
        }

        let raw = std::fs::read_to_string(&toml_path).map_err(MarsError::from)?;
        let raw_def: RawHookDef = toml::from_str(&raw).map_err(|e| {
            MarsError::Config(ConfigError::Invalid {
                message: format!("failed to parse {}: {e}", toml_path.display()),
            })
        })?;

        // Validate event — reject non-V0 events with a hard error.
        let event = UniversalEvent::parse(&raw_def.event)?;

        items.push(ParsedHookItem {
            def: HookDef {
                name: raw_def.name,
                event,
                visibility: raw_def.visibility,
                targets: raw_def.targets,
                action: raw_def.action,
                order: raw_def.order,
            },
            source_name: source_name.to_string(),
            package_depth,
            dep_decl_order,
            package_root: package_root.to_path_buf(),
        });
    }

    Ok(items)
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// A hook with a fully computed sort key for deterministic ordering.
#[derive(Debug, Clone)]
pub struct OrderedHook {
    pub item: ParsedHookItem,
    /// Sort key: (depth, dep_decl_order, order_field, name)
    pub sort_key: (usize, usize, i32, String),
}

/// Order hooks by the deterministic total order defined in the spec:
///
/// 1. package depth (root first, depth 0 < depth 1 < ...)
/// 2. dependency declaration order within the same depth
/// 3. explicit `order` field (lower = earlier; default 0)
/// 4. hook name (lexicographic, final tie-breaker)
pub fn order_hooks(items: Vec<ParsedHookItem>) -> Vec<OrderedHook> {
    let mut ordered: Vec<OrderedHook> = items
        .into_iter()
        .map(|item| {
            let sort_key = (
                item.package_depth,
                item.dep_decl_order,
                item.def.order,
                item.def.name.clone(),
            );
            OrderedHook { item, sort_key }
        })
        .collect();

    ordered.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));
    ordered
}

// ---------------------------------------------------------------------------
// Lossiness classification and target translation
// ---------------------------------------------------------------------------

/// How well a universal hook event maps to a target's native hook mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossinessKind {
    /// Native target has the same semantics.
    Exact,
    /// Native target has a nearby semantic equivalent.
    Approximate,
    /// No native equivalent — the hook entry will be dropped with a warning.
    Dropped,
}

/// Result of translating a hook for a specific target.
#[derive(Debug, Clone)]
pub struct TranslatedHook {
    pub hook: OrderedHook,
    pub lossiness: LossinessKind,
    /// Native event name in the target (None when Dropped).
    pub native_event: Option<String>,
}

/// Translate an ordered hook for a specific target root.
///
/// Lossiness table (Claude):
///   session.start → SessionStart (exact)
///   session.end   → SessionStop  (approximate — Claude uses Stop not End)
///   tool.pre      → PreToolUse   (exact)
///   tool.post     → PostToolUse  (exact)
///
/// Lossiness table (Codex):
///   All events → approximate (Codex hook config is structural, not event-named)
///
/// Lossiness table (OpenCode):
///   All events → approximate (plugin hooks)
///
/// Lossiness table (Cursor):
///   All events → dropped (limited/undocumented hook surface)
///
/// Lossiness table (Pi):
///   All events → dropped (no native hook support)
pub fn translate_hook_for_target(hook: OrderedHook, target_root: &str) -> TranslatedHook {
    let (lossiness, native_event) = classify_for_target(hook.item.def.event.clone(), target_root);
    TranslatedHook {
        hook,
        lossiness,
        native_event,
    }
}

fn classify_for_target(
    event: UniversalEvent,
    target_root: &str,
) -> (LossinessKind, Option<String>) {
    match target_root {
        ".claude" => match event {
            UniversalEvent::SessionStart => {
                (LossinessKind::Exact, Some("SessionStart".to_string()))
            }
            UniversalEvent::SessionEnd => {
                // Claude uses SessionStop, not SessionEnd — close but not exact.
                (LossinessKind::Approximate, Some("SessionStop".to_string()))
            }
            UniversalEvent::ToolPre => (LossinessKind::Exact, Some("PreToolUse".to_string())),
            UniversalEvent::ToolPost => (LossinessKind::Exact, Some("PostToolUse".to_string())),
        },
        ".codex" => {
            // Codex uses structural hook entries, not named events — approximate for all.
            let codex_event = match event {
                UniversalEvent::SessionStart => "start",
                UniversalEvent::SessionEnd => "stop",
                UniversalEvent::ToolPre => "pre-exec",
                UniversalEvent::ToolPost => "post-exec",
            };
            (LossinessKind::Approximate, Some(codex_event.to_string()))
        }
        ".opencode" => {
            let opencode_event = match event {
                UniversalEvent::SessionStart => "session:start",
                UniversalEvent::SessionEnd => "session:end",
                UniversalEvent::ToolPre => "tool:before",
                UniversalEvent::ToolPost => "tool:after",
            };
            (LossinessKind::Approximate, Some(opencode_event.to_string()))
        }
        ".cursor" | ".pi" => {
            // No native hook surface.
            (LossinessKind::Dropped, None)
        }
        _ => (LossinessKind::Dropped, None),
    }
}

/// Translate all hooks for a target root, filtering to those that apply.
///
/// Dropped hooks emit a log-level warning (callers should emit diagnostics).
/// Returns both non-dropped and dropped entries so callers can report lossiness.
pub fn translate_hooks_for_target(
    ordered: Vec<OrderedHook>,
    target_root: &str,
) -> Vec<TranslatedHook> {
    ordered
        .into_iter()
        .filter(|h| {
            h.item.def.targets.is_empty() || h.item.def.targets.iter().any(|t| t == target_root)
        })
        .map(|h| translate_hook_for_target(h, target_root))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_hook_toml_dir(dir: &Path, hook_name: &str, toml: &str) {
        let hook_dir = dir.join("hooks").join(hook_name);
        std::fs::create_dir_all(&hook_dir).unwrap();
        std::fs::write(hook_dir.join("hook.toml"), toml).unwrap();
    }

    fn make_script_hook(dir: &Path, hook_name: &str, event: &str) {
        make_hook_toml_dir(
            dir,
            hook_name,
            &format!(
                r#"
name = "{hook_name}"
event = "{event}"
[action]
kind = "script"
path = "./run.sh"
"#
            ),
        );
    }

    #[test]
    fn discover_finds_hook_items() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "audit", "tool.pre");

        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].def.name, "audit");
        assert_eq!(items[0].def.event, UniversalEvent::ToolPre);
    }

    #[test]
    fn discover_empty_when_no_hooks_dir() {
        let tmp = TempDir::new().unwrap();
        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn discover_rejects_non_v0_event() {
        let tmp = TempDir::new().unwrap();
        make_hook_toml_dir(
            tmp.path(),
            "bad-hook",
            r#"
name = "bad"
event = "spawn.created"
[action]
kind = "script"
path = "./run.sh"
"#,
        );
        let result = discover_hook_items(tmp.path(), "base", 0, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("spawn.created"));
    }

    #[test]
    fn universal_event_parse_accepts_all_v0() {
        assert!(UniversalEvent::parse("session.start").is_ok());
        assert!(UniversalEvent::parse("session.end").is_ok());
        assert!(UniversalEvent::parse("tool.pre").is_ok());
        assert!(UniversalEvent::parse("tool.post").is_ok());
    }

    #[test]
    fn universal_event_parse_rejects_unknown() {
        let err = UniversalEvent::parse("work.start").unwrap_err();
        assert!(err.to_string().contains("work.start"));
    }

    #[test]
    fn order_hooks_depth_first() {
        let tmp_root = TempDir::new().unwrap();
        let tmp_dep = TempDir::new().unwrap();

        make_script_hook(tmp_root.path(), "root-hook", "tool.pre");
        make_script_hook(tmp_dep.path(), "dep-hook", "tool.pre");

        let mut root_items = discover_hook_items(tmp_root.path(), "root", 0, 0).unwrap();
        let dep_items = discover_hook_items(tmp_dep.path(), "dep", 1, 0).unwrap();
        root_items.extend(dep_items);

        let ordered = order_hooks(root_items);
        assert_eq!(ordered[0].item.def.name, "root-hook");
        assert_eq!(ordered[1].item.def.name, "dep-hook");
    }

    #[test]
    fn order_hooks_explicit_order_field() {
        let tmp = TempDir::new().unwrap();
        make_hook_toml_dir(
            tmp.path(),
            "hook-b",
            r#"
name = "hook-b"
event = "tool.pre"
order = 10
[action]
kind = "script"
path = "./b.sh"
"#,
        );
        make_hook_toml_dir(
            tmp.path(),
            "hook-a",
            r#"
name = "hook-a"
event = "tool.pre"
order = 5
[action]
kind = "script"
path = "./a.sh"
"#,
        );

        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);
        // hook-a has lower order (5) so it runs first.
        assert_eq!(ordered[0].item.def.name, "hook-a");
        assert_eq!(ordered[1].item.def.name, "hook-b");
    }

    #[test]
    fn order_hooks_name_as_tiebreaker() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "zebra", "tool.pre");
        make_script_hook(tmp.path(), "alpha", "tool.pre");

        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);
        // Same depth, same order field (0), name tiebreaker: alpha < zebra.
        assert_eq!(ordered[0].item.def.name, "alpha");
        assert_eq!(ordered[1].item.def.name, "zebra");
    }

    #[test]
    fn translate_claude_tool_pre_is_exact() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "audit", "tool.pre");
        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);
        let translated = translate_hook_for_target(ordered.into_iter().next().unwrap(), ".claude");
        assert_eq!(translated.lossiness, LossinessKind::Exact);
        assert_eq!(translated.native_event.as_deref(), Some("PreToolUse"));
    }

    #[test]
    fn translate_claude_session_end_is_approximate() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "cleanup", "session.end");
        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);
        let translated = translate_hook_for_target(ordered.into_iter().next().unwrap(), ".claude");
        assert_eq!(translated.lossiness, LossinessKind::Approximate);
        assert_eq!(translated.native_event.as_deref(), Some("SessionStop"));
    }

    #[test]
    fn translate_cursor_is_dropped() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "hook", "tool.pre");
        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);
        let translated = translate_hook_for_target(ordered.into_iter().next().unwrap(), ".cursor");
        assert_eq!(translated.lossiness, LossinessKind::Dropped);
        assert!(translated.native_event.is_none());
    }

    #[test]
    fn translate_hooks_filters_by_target() {
        let tmp = TempDir::new().unwrap();
        make_hook_toml_dir(
            tmp.path(),
            "claude-only",
            r#"
name = "claude-only"
event = "tool.pre"
targets = [".claude"]
[action]
kind = "script"
path = "./run.sh"
"#,
        );
        make_script_hook(tmp.path(), "all-targets", "tool.post");

        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let ordered = order_hooks(items);

        let claude_hooks = translate_hooks_for_target(ordered.clone(), ".claude");
        assert_eq!(claude_hooks.len(), 2);

        let codex_hooks = translate_hooks_for_target(ordered, ".codex");
        assert_eq!(codex_hooks.len(), 1);
        assert_eq!(codex_hooks[0].hook.item.def.name, "all-targets");
    }

    #[test]
    fn ordering_is_deterministic_across_multiple_calls() {
        let tmp = TempDir::new().unwrap();
        make_script_hook(tmp.path(), "c-hook", "tool.pre");
        make_script_hook(tmp.path(), "a-hook", "session.start");
        make_script_hook(tmp.path(), "b-hook", "tool.post");

        let items = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
        let first: Vec<String> = order_hooks(items.clone())
            .iter()
            .map(|h| h.item.def.name.clone())
            .collect();
        for _ in 0..5 {
            let items2 = discover_hook_items(tmp.path(), "base", 0, 0).unwrap();
            let current: Vec<String> = order_hooks(items2)
                .iter()
                .map(|h| h.item.def.name.clone())
                .collect();
            assert_eq!(first, current);
        }
    }
}
