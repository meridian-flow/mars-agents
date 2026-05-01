/// Per-target compilation adapters.
///
/// Each target root (`.agents`, `.claude`, `.codex`, `.opencode`, `.pi`, `.cursor`)
/// has an adapter that knows how to lower agents, format config entries, translate
/// hooks, and resolve model aliases for that target.
///
/// The adapter boundary isolates all per-target branching here, keeping shared
/// compiler code free of `if target == ...` chains.
pub mod agents;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod opencode;
pub mod pi;

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::error::MarsError;
use crate::lock::ItemKind;
use crate::types::DestPath;

/// A config entry to be written to a target's config file.
///
/// Adapters consume these entries to write or update target-specific config
/// files (MCP JSON, hooks in settings.json, etc.).
#[derive(Debug, Clone)]
pub enum ConfigEntry {
    /// An MCP server entry to register in the target's MCP config file.
    McpServer(McpServerEntry),
    /// A hook binding to register in the target's hook config.
    Hook(HookEntry),
}

impl ConfigEntry {
    /// Stable identity key for this entry (used by stale-cleanup logic).
    pub fn key(&self) -> String {
        match self {
            ConfigEntry::McpServer(e) => format!("mcp:{}", e.name),
            ConfigEntry::Hook(e) => format!("hook:{}:{}", e.event, e.name),
        }
    }
}

/// An MCP server entry ready to be written into a target config file.
///
/// Env values are variable names (symbolic). Adapters translate them to the
/// target's interpolation syntax (e.g. `${VAR}` for Claude, plain name for Codex).
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    /// Server name as it appears in the target config.
    pub name: String,
    /// Launch command.
    pub command: String,
    /// Launch arguments.
    pub args: Vec<String>,
    /// Env vars: config key → environment variable name (symbolic, never resolved).
    pub env: IndexMap<String, String>,
}

/// A hook binding entry ready to be written into a target config file.
#[derive(Debug, Clone)]
pub struct HookEntry {
    /// Hook name (for identification — two hooks with the same name from
    /// different packages are both executed; hooks are additive).
    pub name: String,
    /// Universal event name (e.g. "tool.pre").
    pub event: String,
    /// Native event name for this target (e.g. "PreToolUse" for Claude).
    pub native_event: String,
    /// Script path to execute, relative to the target directory.
    pub script_path: String,
    /// Explicit ordering hint (lower = earlier).
    pub order: i32,
}

/// Per-target compilation adapter.
///
/// Implementations encapsulate all per-target knowledge:
/// - Which item kinds this target accepts
/// - Default destination path layout
/// - Config-entry format (future: MCP, hooks, model aliases)
///
/// The trait is split into file-output surfaces and config-entry surfaces so
/// parallel pipeline lanes can own disjoint write responsibilities without
/// interfering with each other.
///
/// # Object safety
/// All methods take `&self` and return concrete types to ensure the trait can
/// be used as `dyn TargetAdapter`.
pub trait TargetAdapter: std::fmt::Debug + Send + Sync {
    /// Target root name (e.g., `.agents`, `.claude`, `.codex`).
    fn name(&self) -> &str;

    // -----------------------------------------------------------------------
    // Path resolution
    // -----------------------------------------------------------------------

    /// Default destination path for an item of the given kind and name.
    ///
    /// Returns `None` if this target does not accept the item kind. The
    /// compiler MUST skip items for which this returns `None`.
    fn default_dest_path(&self, kind: ItemKind, name: &str) -> Option<DestPath>;

    // -----------------------------------------------------------------------
    // Config-file writing
    // -----------------------------------------------------------------------

    /// Write config entries (MCP servers, hooks) to this target's config file.
    ///
    /// Returns the paths of files written, for lock tracking.
    /// Default: no-op — targets that don't use a config file leave this as-is.
    fn write_config_entries(
        &self,
        _entries: &[ConfigEntry],
        _target_dir: &Path,
    ) -> Result<Vec<PathBuf>, MarsError> {
        Ok(Vec::new())
    }

    /// Remove stale config entries from this target's config file.
    ///
    /// `entry_keys` are the `ConfigEntry::key` values to remove.
    /// Default: no-op.
    fn remove_config_entries(
        &self,
        _entry_keys: &[String],
        _target_dir: &Path,
    ) -> Result<(), MarsError> {
        Ok(())
    }
}

/// Registry of target adapters, keyed by target root name.
///
/// Constructed once per sync run. Adapters are registered at startup; no
/// dynamic registration is needed.
pub struct TargetRegistry {
    adapters: Vec<Box<dyn TargetAdapter>>,
}

impl TargetRegistry {
    /// Build a registry containing all built-in target adapters.
    pub fn new() -> Self {
        Self {
            adapters: vec![
                Box::new(agents::AgentsAdapter),
                Box::new(claude::ClaudeAdapter),
                Box::new(codex::CodexAdapter),
                Box::new(opencode::OpencodeAdapter),
                Box::new(pi::PiAdapter),
                Box::new(cursor::CursorAdapter),
            ],
        }
    }

    /// Look up an adapter by target root name.
    ///
    /// Returns `None` if no adapter is registered for the given name. Callers
    /// may fall back to a default behavior (currently: pass-through copy) when
    /// no adapter is found.
    pub fn get(&self, name: &str) -> Option<&dyn TargetAdapter> {
        self.adapters
            .iter()
            .find(|a| a.name() == name)
            .map(|a| a.as_ref())
    }

    /// Iterate over all registered adapters.
    pub fn iter(&self) -> impl Iterator<Item = &dyn TargetAdapter> {
        self.adapters.iter().map(|a| a.as_ref())
    }
}

impl Default for TargetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_all_builtin_adapters() {
        let registry = TargetRegistry::new();
        let names: Vec<&str> = registry.iter().map(|a| a.name()).collect();
        assert!(names.contains(&".agents"));
        assert!(names.contains(&".claude"));
        assert!(names.contains(&".codex"));
        assert!(names.contains(&".opencode"));
        assert!(names.contains(&".pi"));
        assert!(names.contains(&".cursor"));
    }

    #[test]
    fn registry_get_returns_adapter_by_name() {
        let registry = TargetRegistry::new();
        let adapter = registry.get(".agents").unwrap();
        assert_eq!(adapter.name(), ".agents");
    }

    #[test]
    fn registry_get_unknown_name_returns_none() {
        let registry = TargetRegistry::new();
        assert!(registry.get(".unknown-target").is_none());
    }

    #[test]
    fn agents_adapter_default_dest_path_agent() {
        let registry = TargetRegistry::new();
        let adapter = registry.get(".agents").unwrap();
        let path = adapter.default_dest_path(ItemKind::Agent, "coder").unwrap();
        assert_eq!(path.as_str(), "agents/coder.md");
    }

    #[test]
    fn agents_adapter_default_dest_path_skill() {
        let registry = TargetRegistry::new();
        let adapter = registry.get(".agents").unwrap();
        let path = adapter
            .default_dest_path(ItemKind::Skill, "planning")
            .unwrap();
        assert_eq!(path.as_str(), "skills/planning");
    }
}
