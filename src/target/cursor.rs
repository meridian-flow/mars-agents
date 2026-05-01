/// `.cursor` target adapter.
///
/// Handles MCP server registration for the Cursor IDE.
///
/// Cursor-native lowering:
/// - MCP: writes to `mcp.json` (mcpServers section), env vars as `${env:VAR}` syntax
/// - Hooks: dropped — Cursor has limited/undocumented hook surface (lossiness: dropped)
use std::path::{Path, PathBuf};

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::lock::ItemKind;
use crate::types::DestPath;

use super::{ConfigEntry, McpServerEntry, TargetAdapter};

#[derive(Debug)]
pub struct CursorAdapter;

impl TargetAdapter for CursorAdapter {
    fn name(&self) -> &str {
        ".cursor"
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

        // Hooks are dropped for Cursor — no native hook surface.
        // Callers should have already emitted lossiness diagnostics for dropped hooks.

        if mcp_servers.is_empty() {
            return Ok(Vec::new());
        }

        let path = write_cursor_mcp_json(target_dir, &mcp_servers)?;
        Ok(vec![path])
    }

    fn remove_config_entries(
        &self,
        entry_keys: &[String],
        target_dir: &Path,
    ) -> Result<(), MarsError> {
        remove_cursor_mcp_entries(entry_keys, target_dir)
    }
}

impl CursorAdapter {
    /// Emit diagnostics for any hook entries in `entries` (all dropped for Cursor).
    ///
    /// Called by the compiler before `write_config_entries` so that the
    /// lossiness is reported even though hooks are silently skipped in write.
    pub fn emit_hook_lossiness_diagnostics(entries: &[ConfigEntry], diag: &mut DiagnosticCollector) {
        for entry in entries {
            if let ConfigEntry::Hook(hook) = entry {
                diag.warn(
                    "hook-dropped",
                    format!(
                        "hook `{}` (event `{}`) dropped for target `.cursor` — \
                         Cursor has no native hook support",
                        hook.name, hook.event
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor MCP — `mcp.json` format
// ---------------------------------------------------------------------------
//
// Cursor uses `${env:VAR_NAME}` interpolation syntax for env vars.
// {
//   "mcpServers": {
//     "server-name": {
//       "command": "...",
//       "args": [...],
//       "env": { "KEY": "${env:VAR_NAME}" }
//     }
//   }
// }

fn write_cursor_mcp_json(
    target_dir: &Path,
    servers: &[&McpServerEntry],
) -> Result<PathBuf, MarsError> {
    let path = target_dir.join("mcp.json");

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

        // Cursor env: `${env:VAR_NAME}` interpolation syntax.
        if !server.env.is_empty() {
            let env_obj: serde_json::Map<String, serde_json::Value> = server
                .env
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        serde_json::Value::String(format!("${{env:{v}}}")),
                    )
                })
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

fn remove_cursor_mcp_entries(entry_keys: &[String], target_dir: &Path) -> Result<(), MarsError> {
    let path = target_dir.join("mcp.json");
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{HookEntry, McpServerEntry};
    use indexmap::IndexMap;
    use tempfile::TempDir;

    fn make_mcp_entry(name: &str, env_var: Option<(&str, &str)>) -> ConfigEntry {
        let mut env = IndexMap::new();
        if let Some((k, v)) = env_var {
            env.insert(k.to_string(), v.to_string());
        }
        ConfigEntry::McpServer(McpServerEntry {
            name: name.to_string(),
            command: "npx".to_string(),
            args: vec![],
            env,
        })
    }

    fn make_hook_entry(name: &str) -> ConfigEntry {
        ConfigEntry::Hook(HookEntry {
            name: name.to_string(),
            event: "tool.pre".to_string(),
            native_event: "PreToolUse".to_string(),
            script_path: "/hooks/run.sh".to_string(),
            order: 0,
        })
    }

    #[test]
    fn write_mcp_creates_mcp_json() {
        let tmp = TempDir::new().unwrap();
        let adapter = CursorAdapter;
        let entries = vec![make_mcp_entry("context7", None)];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        assert_eq!(written.len(), 1);
        assert!(tmp.path().join("mcp.json").exists());

        let raw = std::fs::read_to_string(tmp.path().join("mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["context7"].is_object());
    }

    #[test]
    fn write_mcp_env_uses_cursor_interpolation() {
        let tmp = TempDir::new().unwrap();
        let adapter = CursorAdapter;
        let entries = vec![make_mcp_entry("server", Some(("API_KEY", "MY_SECRET")))];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Cursor uses ${env:VAR_NAME} interpolation syntax
        assert_eq!(
            json["mcpServers"]["server"]["env"]["API_KEY"],
            "${env:MY_SECRET}"
        );
    }

    #[test]
    fn write_hooks_dropped_no_file_written() {
        let tmp = TempDir::new().unwrap();
        let adapter = CursorAdapter;
        let entries = vec![make_hook_entry("audit")];
        let written = adapter.write_config_entries(&entries, tmp.path()).unwrap();
        // Hooks are dropped — no file written.
        assert!(written.is_empty());
        assert!(!tmp.path().join("settings.json").exists());
    }

    #[test]
    fn hook_lossiness_emits_diagnostic() {
        let entries = vec![make_hook_entry("audit")];
        let mut diag = crate::diagnostic::DiagnosticCollector::new();
        CursorAdapter::emit_hook_lossiness_diagnostics(&entries, &mut diag);
        let collected = diag.drain();
        assert_eq!(collected.len(), 1);
        assert!(collected[0].message.contains("dropped"));
    }

    #[test]
    fn remove_mcp_entries_preserves_others() {
        let tmp = TempDir::new().unwrap();
        let adapter = CursorAdapter;
        let entries = vec![make_mcp_entry("to-remove", None), make_mcp_entry("to-keep", None)];
        adapter.write_config_entries(&entries, tmp.path()).unwrap();

        adapter
            .remove_config_entries(&["mcp:to-remove".to_string()], tmp.path())
            .unwrap();

        let raw = std::fs::read_to_string(tmp.path().join("mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json["mcpServers"]["to-remove"].is_null());
        assert!(json["mcpServers"]["to-keep"].is_object());
    }
}
