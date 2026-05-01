/// `.codex` target adapter.
///
/// Handles MCP server registration and hook binding for the Codex harness.
///
/// Codex-native lowering:
/// - MCP: writes to `codex_mcp.json` (mcpServers section), env vars as plain names
/// - Hooks: writes to `codex_hooks.json` with structural hook entries
use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::lock::ItemKind;
use crate::types::DestPath;

use super::{ConfigEntry, HookEntry, McpServerEntry, TargetAdapter, hook_command};

#[derive(Debug)]
pub struct CodexAdapter;

impl TargetAdapter for CodexAdapter {
    fn name(&self) -> &str {
        ".codex"
    }

    fn default_dest_path(&self, kind: ItemKind, name: &str) -> Option<DestPath> {
        match kind {
            ItemKind::Skill => Some(DestPath::from(format!("skills/{name}").as_str())),
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
            let path = write_codex_mcp_json(target_dir, &mcp_servers)?;
            written.push(path);
        }

        if !hooks.is_empty() {
            let path = write_codex_hooks_json(target_dir, &hooks)?;
            written.push(path);
        }

        Ok(written)
    }

    fn remove_config_entries(
        &self,
        entry_keys: &[String],
        target_dir: &Path,
    ) -> Result<(), MarsError> {
        remove_codex_mcp_entries(entry_keys, target_dir)?;
        remove_codex_hook_entries(entry_keys, target_dir)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Codex MCP — `codex_mcp.json` format
// ---------------------------------------------------------------------------
//
// Codex uses plain environment variable names (no interpolation syntax).
// Format:
// {
//   "mcpServers": {
//     "server-name": {
//       "command": "...",
//       "args": [...],
//       "env": ["ENV_VAR_NAME", ...]   ← list of var names, not map
//     }
//   }
// }

fn write_codex_mcp_json(
    target_dir: &Path,
    servers: &[&McpServerEntry],
) -> Result<PathBuf, MarsError> {
    let path = target_dir.join("codex_mcp.json");

    let mut root: serde_json::Value = if path.is_file() {
        let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

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

        // Codex env: list of variable names (not a map with values).
        if !server.env.is_empty() {
            let env_list: Vec<serde_json::Value> = server
                .env
                .values()
                .map(|v| serde_json::Value::String(v.clone()))
                .collect();
            entry["env"] = serde_json::Value::Array(env_list);
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

fn remove_codex_mcp_entries(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join("codex_mcp.json");
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
// Codex hooks — `codex_hooks.json` format
// ---------------------------------------------------------------------------
//
// Structural hook entries — Codex uses event → command list mapping.
// {
//   "hooks": {
//     "pre-exec": ["bash /path/to/script.sh"],
//     "post-exec": [...]
//   }
// }

fn write_codex_hooks_json(target_dir: &Path, hooks: &[&HookEntry]) -> Result<PathBuf, MarsError> {
    let path = target_dir.join("codex_hooks.json");

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
        let command = hook_command(&hook.script_path);
        let native_event = hook.native_event.clone();
        hooks_map
            .entry(native_event.clone())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                MarsError::Config(crate::error::ConfigError::Invalid {
                    message: format!("{}: hooks.{native_event} is not an array", path.display()),
                })
            })?
            .push(serde_json::Value::String(command));
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(path)
}

fn remove_codex_hook_entries(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join("codex_hooks.json");
    if !path.is_file() {
        return Ok(());
    }

    let hook_names: Vec<&str> = entry_keys
        .iter()
        .filter_map(|k| {
            let rest = k.strip_prefix("hook:")?;
            rest.splitn(2, ':').nth(1)
        })
        .collect();

    if hook_names.is_empty() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(hooks_map) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|v| v.as_object_mut())
    {
        for event_hooks in hooks_map.values_mut() {
            if let Some(arr) = event_hooks.as_array_mut() {
                arr.retain(|cmd| {
                    let cmd_str = cmd.as_str().unwrap_or("");
                    !hook_names.iter().any(|name| {
                        // Exact path-segment match to avoid partial name collisions.
                        let normalized = cmd_str.replace('\\', "/").replace("//", "/");
                        normalized.contains(&format!("/hooks/{name}/"))
                    })
                });
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

    fn make_mcp_entry_with_env(name: &str) -> ConfigEntry {
        let mut env = IndexMap::new();
        env.insert("API_KEY".to_string(), "MY_SECRET".to_string());
        ConfigEntry::McpServer(McpServerEntry {
            name: name.to_string(),
            command: "npx".to_string(),
            args: vec![],
            env,
        })
    }

    fn make_hook_entry(name: &str, native: &str) -> ConfigEntry {
        ConfigEntry::Hook(HookEntry {
            name: name.to_string(),
            event: "tool.pre".to_string(),
            native_event: native.to_string(),
            script_path: format!("/hooks/{name}/run.sh"),
            order: 0,
        })
    }

    #[test]
    fn write_mcp_creates_codex_mcp_json() {
        let tmp = TempDir::new().unwrap();
        let adapter = CodexAdapter;
        let entries = vec![make_mcp_entry("context7")];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        assert_eq!(written.len(), 1);
        assert!(tmp.path().join("codex_mcp.json").exists());

        let raw = std::fs::read_to_string(tmp.path().join("codex_mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["context7"].is_object());
    }

    #[test]
    fn write_mcp_env_as_list_of_var_names() {
        let tmp = TempDir::new().unwrap();
        let adapter = CodexAdapter;
        let entries = vec![make_mcp_entry_with_env("server")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("codex_mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Codex: env is a list of variable names, not a map with values.
        assert!(json["mcpServers"]["server"]["env"].is_array());
        let env_arr = json["mcpServers"]["server"]["env"].as_array().unwrap();
        assert!(env_arr.iter().any(|v| v.as_str() == Some("MY_SECRET")));
    }

    #[test]
    fn write_hooks_creates_codex_hooks_json() {
        let tmp = TempDir::new().unwrap();
        let adapter = CodexAdapter;
        let entries = vec![make_hook_entry("audit", "pre-exec")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("codex_hooks.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["hooks"]["pre-exec"].is_array());
    }

    #[test]
    fn remove_mcp_entries_removes_by_name() {
        let tmp = TempDir::new().unwrap();
        let adapter = CodexAdapter;
        let entries = vec![make_mcp_entry("to-remove"), make_mcp_entry("to-keep")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        adapter
            .remove_config_entries(&["mcp:to-remove".to_string()], tmp.path())
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("codex_mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["to-remove"].is_null());
        assert!(json["mcpServers"]["to-keep"].is_object());
    }

    #[test]
    fn remove_hook_entries_matches_backslash_commands() {
        let tmp = TempDir::new().unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "pre-exec": [
                    "bash \"C:\\\\pkg\\\\hooks\\\\audit\\\\run.sh\"",
                    "bash \"C:\\\\pkg\\\\hooks\\\\audit-extended\\\\run.sh\""
                ]
            }
        });
        std::fs::write(
            tmp.path().join("codex_hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        remove_codex_hook_entries(&["hook:tool.pre:audit".to_string()], tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("codex_hooks.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let hooks = json["hooks"]["pre-exec"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].as_str().unwrap().contains("audit-extended"));
    }
}
