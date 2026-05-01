/// MCP server compiler lane.
///
/// Discovers, parses, validates, and lowers MCP server definitions from
/// package trees into per-target config entries.
///
/// Responsibilities:
/// - Parse `mcp/<name>/mcp.toml` from package roots
/// - Preserve env references symbolically (mars never resolves secrets)
/// - Warn (or error under `--strict`) when an env var is absent at sync time
/// - Detect per-(target_root, mcp_name) collisions: same name in same target = hard error
/// - Produce `MarsTargetMcpEntry` per target for adapter config writing
use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::diagnostic::DiagnosticCollector;
use crate::error::{ConfigError, MarsError};

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

/// A symbolic environment reference.
///
/// `from = "env"` is the only supported kind in V0.
/// The value is never resolved — it flows through as a reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from")]
pub enum EnvRef {
    /// Read from the process environment at the harness's runtime.
    #[serde(rename = "env")]
    Env {
        /// Name of the environment variable.
        var: String,
    },
}

impl EnvRef {
    /// Return the environment variable name for preflight checking.
    pub fn var_name(&self) -> &str {
        match self {
            EnvRef::Env { var } => var.as_str(),
        }
    }
}

/// Parsed content of a single `mcp/<name>/mcp.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerDef {
    /// Server name — matches the directory name by convention but can be
    /// overridden in the TOML file.
    #[serde(default)]
    pub name: Option<String>,
    /// Command to launch the MCP server.
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Symbolic environment references.
    #[serde(default)]
    pub env: indexmap::IndexMap<String, EnvRef>,
    /// Visibility: "local" (default) or "exported".
    /// Exported MCP servers propagate to transitive consumers.
    #[serde(default = "default_visibility")]
    pub visibility: String,
    /// Optional target filter — if absent, applies to all targets.
    #[serde(default)]
    pub targets: Vec<String>,
}

fn default_visibility() -> String {
    "local".to_string()
}

/// A discovered MCP server item with provenance.
#[derive(Debug, Clone)]
pub struct ParsedMcpItem {
    /// Resolved server name (directory name, unless overridden in TOML).
    pub name: String,
    /// Parsed definition.
    pub def: McpServerDef,
    /// Source package name this item came from.
    pub source_name: String,
    /// Depth of the package in the dependency graph (0 = root package).
    pub package_depth: usize,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover MCP server items from a package root.
///
/// Scans `<package_root>/mcp/<name>/mcp.toml` for each subdirectory.
/// Returns the parsed items in directory-sorted order.
pub fn discover_mcp_items(
    package_root: &Path,
    source_name: &str,
    package_depth: usize,
) -> Result<Vec<ParsedMcpItem>, MarsError> {
    let mcp_dir = package_root.join("mcp");
    if !mcp_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&mcp_dir)
        .map_err(MarsError::from)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir_name = entry.file_name();
        let server_name = dir_name.to_string_lossy();
        // Skip hidden directories.
        if server_name.starts_with('.') {
            continue;
        }

        let toml_path = entry.path().join("mcp.toml");
        if !toml_path.is_file() {
            continue;
        }

        let raw = std::fs::read_to_string(&toml_path).map_err(MarsError::from)?;
        let def: McpServerDef = toml::from_str(&raw).map_err(|e| {
            MarsError::Config(ConfigError::Invalid {
                message: format!("failed to parse {}: {e}", toml_path.display()),
            })
        })?;

        // Resolved name: TOML override wins, else directory name.
        let resolved_name = def
            .name
            .as_deref()
            .unwrap_or(&server_name)
            .to_string();

        items.push(ParsedMcpItem {
            name: resolved_name,
            def,
            source_name: source_name.to_string(),
            package_depth,
        });
    }

    Ok(items)
}

// ---------------------------------------------------------------------------
// Env var preflight
// ---------------------------------------------------------------------------

/// Check that env references name variables present in the current environment.
///
/// In normal mode: emits a warning per missing variable.
/// Under `strict`: returns an error for the first missing variable.
pub fn check_env_refs(
    items: &[ParsedMcpItem],
    strict: bool,
    diag: &mut DiagnosticCollector,
) -> Result<(), MarsError> {
    for item in items {
        for (key, env_ref) in &item.def.env {
            let var_name = env_ref.var_name();
            if std::env::var(var_name).is_err() {
                let msg = format!(
                    "MCP server `{}` (from `{}`): env var `{var_name}` (referenced by `{key}`) \
                     is not set — the server may fail at runtime",
                    item.name, item.source_name
                );
                if strict {
                    return Err(MarsError::Config(ConfigError::Invalid { message: msg }));
                }
                diag.warn("mcp-env-missing", msg);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Collision detection
// ---------------------------------------------------------------------------

/// Check for per-target-root MCP name collisions.
///
/// Two packages declaring the same MCP server name for the same target root
/// is a hard error. Cross-target duplicates are allowed.
///
/// `items` — all MCP items across all packages.
/// `target_roots` — the set of target roots that will be configured.
pub fn detect_mcp_collisions(
    items: &[ParsedMcpItem],
    target_roots: &[&str],
) -> Result<(), MarsError> {
    // For each target root, track (mcp_name -> source_name) seen so far.
    let mut seen: HashMap<&str, HashMap<&str, &str>> = HashMap::new();

    for target_root in target_roots {
        seen.insert(target_root, HashMap::new());
    }

    for item in items {
        // Determine which targets this item applies to.
        let applicable: Vec<&str> = if item.def.targets.is_empty() {
            target_roots.to_vec()
        } else {
            item.def
                .targets
                .iter()
                .filter_map(|t| target_roots.iter().find(|&&tr| tr == t.as_str()).copied())
                .collect()
        };

        for target_root in applicable {
            if let Some(per_target) = seen.get_mut(target_root) {
                if let Some(existing_source) = per_target.get(item.name.as_str()) {
                    return Err(MarsError::Config(ConfigError::Invalid {
                        message: format!(
                            "MCP server name collision in target `{target_root}`: \
                             `{}` declared by both `{existing_source}` and `{}`",
                            item.name, item.source_name
                        ),
                    }));
                }
                per_target.insert(item.name.as_str(), item.source_name.as_str());
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Target lowering
// ---------------------------------------------------------------------------

/// A fully lowered MCP server entry ready for a target adapter to write.
#[derive(Debug, Clone)]
pub struct TargetMcpEntry {
    /// Server name as it appears in the target config.
    pub name: String,
    /// Launch command.
    pub command: String,
    /// Launch arguments.
    pub args: Vec<String>,
    /// Env vars: key → variable name (symbolic — adapters write the native form).
    pub env: indexmap::IndexMap<String, String>,
}

impl TargetMcpEntry {
    /// Build from a parsed item.
    pub fn from_parsed(item: &ParsedMcpItem) -> Self {
        let env = item
            .def
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.var_name().to_string()))
            .collect();
        Self {
            name: item.name.clone(),
            command: item.def.command.clone(),
            args: item.def.args.clone(),
            env,
        }
    }
}

/// Lower all MCP items for a specific target root.
///
/// Filters to items that apply to the given target (empty target list = all targets).
pub fn lower_for_target<'a>(
    items: &'a [ParsedMcpItem],
    target_root: &str,
) -> Vec<TargetMcpEntry> {
    items
        .iter()
        .filter(|item| {
            item.def.targets.is_empty() || item.def.targets.iter().any(|t| t == target_root)
        })
        .map(TargetMcpEntry::from_parsed)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_mcp_toml_dir(dir: &Path, server_name: &str, toml: &str) {
        let server_dir = dir.join("mcp").join(server_name);
        std::fs::create_dir_all(&server_dir).unwrap();
        std::fs::write(server_dir.join("mcp.toml"), toml).unwrap();
    }

    #[test]
    fn discover_finds_mcp_items() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "context7",
            r#"
command = "npx"
args = ["-y", "@upstash/context7-mcp@latest"]
"#,
        );

        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "context7");
        assert_eq!(items[0].def.command, "npx");
        assert_eq!(items[0].def.args, &["-y", "@upstash/context7-mcp@latest"]);
    }

    #[test]
    fn discover_empty_when_no_mcp_dir() {
        let tmp = TempDir::new().unwrap();
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn discover_skips_dir_without_mcp_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("mcp/no-toml")).unwrap();
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn discover_respects_name_override() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "dir-name",
            r#"
name = "custom-name"
command = "node"
"#,
        );
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        assert_eq!(items[0].name, "custom-name");
    }

    #[test]
    fn discover_parses_env_refs() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "api-server",
            r#"
command = "npx"
[env]
API_KEY = { from = "env", var = "MY_API_KEY" }
"#,
        );
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        assert_eq!(items[0].def.env.len(), 1);
        let env_ref = &items[0].def.env["API_KEY"];
        assert_eq!(env_ref.var_name(), "MY_API_KEY");
    }

    #[test]
    fn check_env_refs_warns_when_missing() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "server",
            r#"
command = "npx"
[env]
KEY = { from = "env", var = "MARS_TEST_DEFINITELY_NOT_SET_XYZ123" }
"#,
        );
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        let mut diag = DiagnosticCollector::new();
        check_env_refs(&items, false, &mut diag).unwrap();
        let collected = diag.drain();
        assert_eq!(collected.len(), 1);
        assert!(collected[0].message.contains("MARS_TEST_DEFINITELY_NOT_SET_XYZ123"));
    }

    #[test]
    fn check_env_refs_strict_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "server",
            r#"
command = "npx"
[env]
KEY = { from = "env", var = "MARS_TEST_DEFINITELY_NOT_SET_XYZ456" }
"#,
        );
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        let mut diag = DiagnosticCollector::new();
        let result = check_env_refs(&items, true, &mut diag);
        assert!(result.is_err());
    }

    #[test]
    fn detect_collisions_same_name_same_target_errors() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        make_mcp_toml_dir(tmp_a.path(), "context7", "command = \"npx\"");
        make_mcp_toml_dir(tmp_b.path(), "context7", "command = \"npx\"");

        let items_a = discover_mcp_items(tmp_a.path(), "source-a", 0).unwrap();
        let items_b = discover_mcp_items(tmp_b.path(), "source-b", 1).unwrap();
        let all: Vec<_> = items_a.into_iter().chain(items_b).collect();

        let result = detect_mcp_collisions(&all, &[".claude"]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("context7"));
    }

    #[test]
    fn detect_collisions_same_name_different_targets_allowed() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp_a.path(),
            "context7",
            "command = \"npx\"\ntargets = [\".claude\"]",
        );
        make_mcp_toml_dir(
            tmp_b.path(),
            "context7",
            "command = \"npx\"\ntargets = [\".codex\"]",
        );

        let items_a = discover_mcp_items(tmp_a.path(), "source-a", 0).unwrap();
        let items_b = discover_mcp_items(tmp_b.path(), "source-b", 1).unwrap();
        let all: Vec<_> = items_a.into_iter().chain(items_b).collect();

        // .claude and .codex are different targets — no collision.
        detect_mcp_collisions(&all, &[".claude", ".codex"]).unwrap();
    }

    #[test]
    fn lower_for_target_filters_by_target() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "claude-only",
            "command = \"npx\"\ntargets = [\".claude\"]",
        );
        make_mcp_toml_dir(tmp.path(), "all-targets", "command = \"node\"");

        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();

        let claude_entries = lower_for_target(&items, ".claude");
        assert_eq!(claude_entries.len(), 2);

        let codex_entries = lower_for_target(&items, ".codex");
        assert_eq!(codex_entries.len(), 1);
        assert_eq!(codex_entries[0].name, "all-targets");
    }

    #[test]
    fn env_ref_preserves_symbolic_var_name() {
        let tmp = TempDir::new().unwrap();
        make_mcp_toml_dir(
            tmp.path(),
            "server",
            r#"
command = "npx"
[env]
TOKEN = { from = "env", var = "SECRET_TOKEN" }
"#,
        );
        let items = discover_mcp_items(tmp.path(), "base", 0).unwrap();
        let entry = TargetMcpEntry::from_parsed(&items[0]);
        // The env map carries the variable name, not the resolved value.
        assert_eq!(entry.env["TOKEN"], "SECRET_TOKEN");
    }
}
