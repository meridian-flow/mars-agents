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

use super::{ConfigEntry, HookEntry, McpServerEntry, TargetAdapter};

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
            let command = format!("bash {}", hook.script_path);
            hooks_map
                .entry(hook.native_event.clone())
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .unwrap()
                .push(serde_json::Value::String(command));
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
    let hook_names: Vec<&str> = entry_keys
        .iter()
        .filter_map(|k| {
            let rest = k.strip_prefix("hook:")?;
            rest.splitn(2, ':').nth(1)
        })
        .collect();

    if !hook_names.is_empty() {
        if let Some(hooks_map) = root_obj.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            for event_hooks in hooks_map.values_mut() {
                if let Some(arr) = event_hooks.as_array_mut() {
                    arr.retain(|cmd| {
                        let cmd_str = cmd.as_str().unwrap_or("");
                        !hook_names.iter().any(|name| {
                            // Exact path-segment match to avoid partial name collisions.
                            let seg_fwd = format!("/hooks/{name}/");
                            let seg_bwd = format!("\\hooks\\{name}\\");
                            cmd_str.contains(&seg_fwd) || cmd_str.contains(&seg_bwd)
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
