/// `.opencode` target adapter.
///
/// Handles MCP server registration and hook binding for the OpenCode harness.
///
/// OpenCode-native lowering:
/// - MCP: writes to `opencode.json` (mcpServers section), env vars as plain name map
/// - Hooks: writes to `opencode.json` (hooks section with plugin hook format)
use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::lock::ItemKind;
use crate::types::DestPath;

use super::{ConfigEntry, HookEntry, McpServerEntry, TargetAdapter, hook_command};

#[derive(Debug)]
pub struct OpencodeAdapter;

impl TargetAdapter for OpencodeAdapter {
    fn name(&self) -> &str {
        ".opencode"
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

        if mcp_servers.is_empty() && hooks.is_empty() {
            return Ok(Vec::new());
        }

        // OpenCode merges both into a single config file.
        let path = write_opencode_config(target_dir, &mcp_servers, &hooks)?;
        Ok(vec![path])
    }

    fn remove_config_entries(
        &self,
        entry_keys: &[String],
        target_dir: &Path,
    ) -> Result<(), MarsError> {
        remove_opencode_entries(entry_keys, target_dir)
    }
}

// ---------------------------------------------------------------------------
// OpenCode config — `opencode.json` format
// ---------------------------------------------------------------------------
//
// OpenCode uses a single config file with both MCP and hooks:
// {
//   "mcpServers": {
//     "server-name": {
//       "command": "...",
//       "args": [...],
//       "env": { "KEY": "VAR_NAME" }   ← plain var name, no interpolation
//     }
//   },
//   "hooks": {
//     "session:start": ["bash /path/to/script.sh"],
//     "tool:before": [...]
//   }
// }

fn write_opencode_config(
    target_dir: &Path,
    servers: &[&McpServerEntry],
    hooks: &[&HookEntry],
) -> Result<PathBuf, MarsError> {
    let path = target_dir.join("opencode.json");

    let mut root: serde_json::Value = if path.is_file() {
        let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let root_obj = root.as_object_mut().ok_or_else(|| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("{} is not a JSON object", path.display()),
        })
    })?;

    // MCP servers
    if !servers.is_empty() {
        let mcp_obj = root_obj
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

            // OpenCode: env as plain name map (no interpolation)
            if !server.env.is_empty() {
                let env_obj: serde_json::Map<String, serde_json::Value> = server
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                entry["env"] = serde_json::Value::Object(env_obj);
            }

            mcp_map.insert(server.name.clone(), entry);
        }
    }

    // Hooks
    if !hooks.is_empty() {
        let hooks_obj = root_obj
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}));
        let hooks_map = hooks_obj.as_object_mut().ok_or_else(|| {
            MarsError::Config(crate::error::ConfigError::Invalid {
                message: format!("{}: hooks is not an object", path.display()),
            })
        })?;

        for hook in hooks {
            let command = hook_command(&hook.script_path);
            let native_event = hook.native_event.clone();
            let event_hooks = hooks_map
                .entry(native_event.clone())
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| {
                    MarsError::Config(crate::error::ConfigError::Invalid {
                        message: format!(
                            "{}: hooks.{native_event} is not an array",
                            path.display()
                        ),
                    })
                })?;
            remove_managed_hook_commands(event_hooks, &hook.name);
            event_hooks.push(serde_json::Value::String(command));
        }
    }

    let content = serde_json::to_string_pretty(&root).map_err(|e| {
        MarsError::Config(crate::error::ConfigError::Invalid {
            message: format!("failed to serialize {}: {e}", path.display()),
        })
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())?;

    Ok(path)
}

fn remove_managed_hook_commands(commands: &mut Vec<serde_json::Value>, hook_name: &str) {
    commands.retain(|cmd| {
        cmd.as_str()
            .map(|cmd| !is_managed_hook_command_for(cmd, hook_name))
            .unwrap_or(true)
    });
}

fn is_managed_hook_command_for(command: &str, hook_name: &str) -> bool {
    let normalized = command.replace('\\', "/").replace("//", "/");
    normalized.contains(&format!("/hooks/{hook_name}/"))
}

fn remove_opencode_entries(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join("opencode.json");
    if !path.is_file() {
        return Ok(());
    }

    let raw = std::fs::read_to_string(&path).map_err(MarsError::from)?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));

    let root_obj = match root.as_object_mut() {
        Some(o) => o,
        None => return Ok(()),
    };

    // Remove MCP entries
    if let Some(mcp_map) = root_obj
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
    {
        for key in entry_keys {
            if let Some(name) = key.strip_prefix("mcp:") {
                mcp_map.remove(name);
            }
        }
    }

    // Remove hook entries
    let hook_keys: Vec<(String, &str)> = entry_keys
        .iter()
        .filter_map(|k| {
            let rest = k.strip_prefix("hook:")?;
            let mut parts = rest.splitn(2, ':');
            let event = parts.next()?;
            let name = parts.next()?;
            Some((opencode_hook_event(event)?.to_string(), name))
        })
        .collect();

    if !hook_keys.is_empty() {
        if let Some(hooks_map) = root_obj.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            for (event, name) in &hook_keys {
                if let Some(arr) = hooks_map.get_mut(event).and_then(|v| v.as_array_mut()) {
                    arr.retain(|cmd| {
                        let cmd_str = cmd.as_str().unwrap_or("");
                        // Exact path-segment match to avoid partial name collisions.
                        !is_managed_hook_command_for(cmd_str, name)
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

fn opencode_hook_event(event: &str) -> Option<&'static str> {
    match event {
        "session.start" => Some("session:start"),
        "session.end" => Some("session:end"),
        "tool.pre" => Some("tool:before"),
        "tool.post" => Some("tool:after"),
        _ => None,
    }
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
        let mut env = IndexMap::new();
        env.insert("TOKEN".to_string(), "MY_TOKEN".to_string());
        ConfigEntry::McpServer(McpServerEntry {
            name: name.to_string(),
            command: "node".to_string(),
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

    fn make_hook_entry_with_path(name: &str, native: &str, script_path: &str) -> ConfigEntry {
        ConfigEntry::Hook(HookEntry {
            name: name.to_string(),
            event: "tool.pre".to_string(),
            native_event: native.to_string(),
            script_path: script_path.to_string(),
            order: 0,
        })
    }

    #[test]
    fn write_config_entries_creates_opencode_json() {
        let tmp = TempDir::new().unwrap();
        let adapter = OpencodeAdapter;
        let entries = vec![make_mcp_entry("context7")];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        assert_eq!(written.len(), 1);
        assert!(tmp.path().join("opencode.json").exists());

        let raw = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["context7"].is_object());
    }

    #[test]
    fn write_mcp_env_as_plain_name_map() {
        let tmp = TempDir::new().unwrap();
        let adapter = OpencodeAdapter;
        let entries = vec![make_mcp_entry("server")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // OpenCode: env is a plain name map (not interpolated)
        assert_eq!(json["mcpServers"]["server"]["env"]["TOKEN"], "MY_TOKEN");
    }

    #[test]
    fn write_hooks_into_same_file() {
        let tmp = TempDir::new().unwrap();
        let adapter = OpencodeAdapter;
        let entries = vec![
            make_mcp_entry("ctx"),
            make_hook_entry("audit", "tool:before"),
        ];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        // Both written to a single file.
        assert_eq!(written.len(), 1);

        let raw = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["ctx"].is_object());
        assert!(json["hooks"]["tool:before"].is_array());
    }

    #[test]
    fn write_hooks_replaces_existing_managed_hook_with_same_event_and_name() {
        let tmp = TempDir::new().unwrap();
        let adapter = OpencodeAdapter;
        adapter
            .write_config_entries(
                &[make_hook_entry_with_path(
                    "audit",
                    "tool:before",
                    "/old/hooks/audit/run.sh",
                )],
                tmp.path(),
            )
            .unwrap();
        adapter
            .write_config_entries(
                &[make_hook_entry_with_path(
                    "audit",
                    "tool:before",
                    "/new/hooks/audit/run.sh",
                )],
                tmp.path(),
            )
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let hooks = json["hooks"]["tool:before"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].as_str().unwrap().contains("/new/hooks/audit/"));
    }

    #[test]
    fn remove_entries_removes_mcp_and_hooks() {
        let tmp = TempDir::new().unwrap();
        let adapter = OpencodeAdapter;
        let entries = vec![make_mcp_entry("to-remove"), make_mcp_entry("to-keep")];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        adapter
            .remove_config_entries(&["mcp:to-remove".to_string()], tmp.path())
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["to-remove"].is_null());
        assert!(json["mcpServers"]["to-keep"].is_object());
    }
}
