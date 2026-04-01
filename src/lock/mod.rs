use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::MarsError;

/// The complete lock file — ownership registry for all managed items.
///
/// Tracks every managed file with provenance and integrity data.
/// TOML format, deterministically ordered (sorted keys) for clean git diffs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Schema version, currently 1.
    pub version: u32,
    pub sources: IndexMap<String, LockedSource>,
    pub items: IndexMap<ItemId, LockedItem>,
}

/// One resolved source in the lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedSource {
    pub url: Option<String>,
    pub path: Option<String>,
    pub version: Option<String>,
    pub commit: Option<String>,
    pub tree_hash: Option<String>,
}

/// One installed item tracked by the lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedItem {
    pub source: String,
    pub kind: ItemKind,
    pub version: Option<String>,
    pub source_checksum: String,
    pub installed_checksum: String,
    pub dest_path: String,
}

/// Stable identity for an installed item — decoupled from source URL.
///
/// Items are identified by `(kind, name)`, not by source URL.
/// If a package moves to a different git host, the item identity is preserved.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ItemId {
    pub kind: ItemKind,
    pub name: String,
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind, self.name)
    }
}

/// Kind of installable item.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Agent,
    Skill,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemKind::Agent => write!(f, "agent"),
            ItemKind::Skill => write!(f, "skill"),
        }
    }
}

/// Load the lock file from the given root directory.
pub fn load(root: &Path) -> Result<LockFile, MarsError> {
    let _ = root;
    todo!()
}

/// Build a new lock file from resolved graph and apply results.
pub fn build(
    _graph: &crate::resolve::ResolvedGraph,
    _applied: &crate::sync::apply::ApplyResult,
    _pruned: &[crate::sync::apply::ActionOutcome],
) -> Result<LockFile, MarsError> {
    todo!()
}

/// Write the lock file atomically to the given root directory.
pub fn write(root: &Path, lock: &LockFile) -> Result<(), MarsError> {
    let _ = (root, lock);
    todo!()
}
