/// `.claude` target adapter.
///
/// Handles MCP server registration in `.mcp.json` and hook binding in
/// `settings.json` within the `.claude/` target directory.
///
/// Claude-native lowering:
/// - MCP: writes to `.mcp.json` (mcpServers section)
/// - Hooks: writes to `settings.json` (hooks section)
/// - Env references: rendered as `${VAR_NAME}` for Claude Desktop config compat
use std::path::{Path, PathBuf};

use crate::error::{ConfigError, MarsError};
use crate::lock::ItemKind;
use crate::types::DestPath;

use super::{ConfigEntry, HookEntry, McpServerEntry, TargetAdapter};

#[derive(Debug)]
pub struct ClaudeAdapter;

impl TargetAdapter for ClaudeAdapter {
    fn name(&self) -> &str {
        ".claude"
    }

    fn default_dest_path(&self, kind: ItemKind, name: &str) -> Option<DestPath> {
        match kind {
            ItemKind::Skill => Some(DestPath::from(format!("skills/{name}").as_str())),
            // Agent, Hook, McpServer, BootstrapDoc routing is deferred.
            _ => None,
        }
    }

    fn write_config_entries(
        &self,
        entries: &[ConfigEntry],
        target_dir: &Path,
    ) -> Result<Vec<PathBuf>, MarsError> {
        let mut written = Vec::new();

        let mcp_servers: Vec<&McpServerEntry> = entries
            .iter()
            .filter_map(|e| {
                if let ConfigEntry::McpServer(s) = e {
                    Some(s)
                } else {
                    None
                }
            })
            .collect();

        let hooks: Vec<&HookEntry> = entries
            .iter()
            .filter_map(|e| {
                if let ConfigEntry::Hook(h) = e {
                    Some(h)
                } else {
                    None
                }
            })
            .collect();

        if !mcp_servers.is_empty() {
            let path = write_mcp_json(target_dir, &mcp_servers)?;
            written.push(path);
        }

        if !hooks.is_empty() {
            let path = write_hooks_settings(target_dir, &hooks)?;
            written.push(path);
        }

        Ok(written)
    }

    fn remove_config_entries(
        &self,
        entry_keys: &[String],
        target_dir: &Path,
    ) -> Result<(), MarsError> {
        remove_mcp_entries_by_key(entry_keys, target_dir)?;
        remove_hook_entries_by_key(entry_keys, target_dir)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MCP JSON — `.mcp.json` format
// ---------------------------------------------------------------------------

/// Write (or merge) MCP servers into `<target_dir>/.mcp.json`.
///
/// The file format is:
/// ```json
/// {
///   "mcpServers": {
///     "server-name": {
///       "command": "npx",
///       "args": [...],
///       "env": { "KEY": "${ENV_VAR}" }
///     }
///   }
/// }
/// ```
///
/// Existing entries with other names are preserved (merge, not replace).
fn write_mcp_json(target_dir: &Path, servers: &[&McpServerEntry]) -> Result<PathBuf, MarsError> {
    let path = target_dir.join(".mcp.json");

    // Load existing config or start fresh.
    let mut root: serde_json::Value = if path.is_file() {
        let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Ensure mcpServers key exists.
    let mcp_obj = root
        .as_object_mut()
        .ok_or_else(|| {
            MarsError::Config(crate::error::ConfigError::Invalid {
                message: format!("{} is not a JSON object", path.display()),
            })
        })?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let mcp_map = mcp_obj.as_object_mut().ok_or_else(|| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("{}: mcpServers is not an object", path.display()),
        })
    })?;

    for server in servers {
        let mut entry = serde_json::json!({
            "command": server.command,
            "args": server.args,
        });

        if !server.env.is_empty() {
            let env_obj: serde_json::Map<String, serde_json::Value> = server
                .env
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(format!("${{{v}}}"))))
                .collect();
            entry["env"] = serde_json::Value::Object(env_obj);
        }

        mcp_map.insert(server.name.clone(), entry);
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(path)
}

/// Remove MCP server entries by key from `.mcp.json`.
fn remove_mcp_entries_by_key(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join(".mcp.json");
    if !path.is_file() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(mcp_map) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|v| v.as_object_mut())
    {
        for key in entry_keys {
            // Keys are "mcp:<name>" — strip the prefix.
            if let Some(name) = key.strip_prefix("mcp:") {
                mcp_map.remove(name);
            }
        }
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Hooks — `settings.json` format
// ---------------------------------------------------------------------------

/// Write (or merge) hook bindings into `<target_dir>/settings.json`.
///
/// Claude hooks live in the `hooks` section:
/// ```json
/// {
///   "hooks": {
///     "PreToolUse": [
///       { "hooks": [{ "type": "command", "command": "bash /path/to/script.sh" }] }
///     ]
///   }
/// }
/// ```
fn write_hooks_settings(target_dir: &Path, hooks: &[&HookEntry]) -> Result<PathBuf, MarsError> {
    let path = target_dir.join("settings.json");

    let mut root: serde_json::Value = if path.is_file() {
        let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hooks_section = root
        .as_object_mut()
        .ok_or_else(|| {
            MarsError::Config(crate::error::ConfigError::Invalid {
                message: format!("{} is not a JSON object", path.display()),
            })
        })?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let hooks_map = hooks_section.as_object_mut().ok_or_else(|| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("{}: hooks is not an object", path.display()),
        })
    })?;

    for hook in hooks {
        let native_event = &hook.native_event;
        let command_entry = serde_json::json!({
            "type": "command",
            "command": format!("bash {}", hook.script_path),
        });
        let hook_binding = serde_json::json!({
            "matcher": "",
            "hooks": [command_entry],
        });

        hooks_map
            .entry(native_event.clone())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .unwrap()
            .push(hook_binding);
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(path)
}

/// Remove hook entries by key from `settings.json`.
///
/// Keys are "hook:<event>:<name>" — we use the native event name to locate
/// the section. Because hooks are additive and the settings.json may contain
/// user-owned entries, we only remove entries we wrote (matched by command path).
fn remove_hook_entries_by_key(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join("settings.json");
    if !path.is_file() {
        return Ok(());
    }

    // For now: if any hook keys are being removed, we reload and remove matching
    // command entries. This is conservative — we only remove entries we know
    // belong to mars-managed hooks.
    let hook_keys: Vec<(&str, &str)> = entry_keys
        .iter()
        .filter_map(|k| {
            let rest = k.strip_prefix("hook:")?;
            let mut parts = rest.splitn(2, ':');
            let event = parts.next()?;
            let name = parts.next()?;
            Some((event, name))
        })
        .collect();

    if hook_keys.is_empty() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));

    // We track removed hooks by their universal event + name in the command string.
    // The format we write is "bash <script_path>", so we match on that prefix.
    if let Some(hooks_map) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|v| v.as_object_mut())
    {
        for (_event, name) in &hook_keys {
            for event_hooks in hooks_map.values_mut() {
                if let Some(arr) = event_hooks.as_array_mut() {
                    arr.retain(|binding| {
                        // Retain if we can't parse it (not ours) or if it doesn't
                        // contain the hook name in any inner command.
                        let Some(inner_hooks) = binding.get("hooks").and_then(|h| h.as_array())
                        else {
                            return true;
                        };
                        !inner_hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|cmd| {
                                    // Exact path-segment match to avoid partial name collisions
                                    // (e.g., "audit" must not match "audit-extended").
                                    let seg_fwd = format!("/hooks/{name}/");
                                    let seg_bwd = format!("\\hooks\\{name}\\");
                                    cmd.contains(&seg_fwd) || cmd.contains(&seg_bwd)
                                })
                                .unwrap_or(false)
                        })
                    });
                }
            }
        }
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use tempfile::TempDir;

    fn make_mcp_entry(name: &str) -> ConfigEntry {
        ConfigEntry::McpServer(McpServerEntry {
            name: name.to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "some-mcp@latest".to_string()],
            env: IndexMap::new(),
        })
    }

    fn make_mcp_entry_with_env(name: &str, env_key: &str, env_var: &str) -> ConfigEntry {
        let mut env = IndexMap::new();
        env.insert(env_key.to_string(), env_var.to_string());
        ConfigEntry::McpServer(McpServerEntry {
            name: name.to_string(),
            command: "npx".to_string(),
            args: vec![],
            env,
        })
    }

    fn make_hook_entry(name: &str, event: &str, native: &str) -> ConfigEntry {
        ConfigEntry::Hook(HookEntry {
            name: name.to_string(),
            event: event.to_string(),
            native_event: native.to_string(),
            script_path: format!("/hooks/{name}/run.sh"),
            order: 0,
        })
    }

    #[test]
    fn write_mcp_creates_mcp_json() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();

        let adapter = ClaudeAdapter;
        let entries = vec![make_mcp_entry("context7")];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();

        assert_eq!(written.len(), 1);
        assert!(tmp.path().join(".mcp.json").exists());

        let raw = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["context7"].is_object());
        assert_eq!(json["mcpServers"]["context7"]["command"], "npx");
    }

    #[test]
    fn write_mcp_merges_with_existing() {
        let tmp = TempDir::new().unwrap();
        let existing = serde_json::json!({
            "mcpServers": { "existing-server": { "command": "old" } }
        });
        std::fs::write(
            tmp.path().join(".mcp.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let adapter = ClaudeAdapter;
        let entries = vec![make_mcp_entry("new-server")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["existing-server"].is_object());
        assert!(json["mcpServers"]["new-server"].is_object());
    }

    #[test]
    fn write_mcp_env_renders_as_interpolation() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeAdapter;
        let entries = vec![make_mcp_entry_with_env("server", "API_KEY", "MY_SECRET")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            json["mcpServers"]["server"]["env"]["API_KEY"],
            "${MY_SECRET}"
        );
    }

    #[test]
    fn write_hooks_creates_settings_json() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeAdapter;
        let entries = vec![make_hook_entry("audit", "tool.pre", "PreToolUse")];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();

        assert_eq!(written.len(), 1);
        assert!(tmp.path().join("settings.json").exists());

        let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["hooks"]["PreToolUse"].is_array());
        assert!(!json["hooks"]["PreToolUse"].as_array().unwrap().is_empty());
    }

    #[test]
    fn remove_mcp_entries_removes_by_name() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeAdapter;
        let entries = vec![make_mcp_entry("context7"), make_mcp_entry("other")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        adapter
            .remove_config_entries(&["mcp:context7".to_string()], tmp.path())
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["context7"].is_null());
        assert!(json["mcpServers"]["other"].is_object());
    }

    #[test]
    fn write_mcp_and_hooks_both_written() {
        let tmp = TempDir::new().unwrap();
        let adapter = ClaudeAdapter;
        let entries = vec![
            make_mcp_entry("context7"),
            make_hook_entry("audit", "tool.pre", "PreToolUse"),
        ];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        assert_eq!(written.len(), 2);
        assert!(tmp.path().join(".mcp.json").exists());
        assert!(tmp.path().join("settings.json").exists());
    }
}
