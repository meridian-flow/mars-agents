use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{LockError, MarsError};

/// The complete lock file — ownership registry for all managed items.
///
/// Tracks every managed file with provenance and integrity data.
/// TOML format, deterministically ordered (sorted keys) for clean git diffs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockFile {
    /// Schema version, currently 1.
    pub version: u32,
    #[serde(default)]
    pub sources: IndexMap<String, LockedSource>,
    #[serde(default)]
    pub items: IndexMap<String, LockedItem>,
}

impl LockFile {
    /// Create a new empty lock file with the current schema version.
    pub fn empty() -> Self {
        LockFile {
            version: 1,
            sources: IndexMap::new(),
            items: IndexMap::new(),
        }
    }
}

/// One resolved source in the lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_hash: Option<String>,
}

/// One installed item tracked by the lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedItem {
    pub source: String,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub source_checksum: String,
    pub installed_checksum: String,
    pub dest_path: String,
}

/// Stable identity for an installed item — decoupled from source URL.
///
/// Items are identified by `(kind, name)`, not by source URL.
/// If a package moves to a different git host, the item identity is preserved.
#[derive(Debug, Clone, Hash, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
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

const LOCK_FILE: &str = "agents.lock";

/// Load the lock file from the given root directory.
///
/// Returns an empty LockFile if the file is absent.
pub fn load(root: &Path) -> Result<LockFile, MarsError> {
    let path = root.join(LOCK_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let lock: LockFile = toml::from_str(&content).map_err(|e| LockError::Corrupt {
                message: format!("failed to parse {}: {e}", path.display()),
            })?;
            Ok(lock)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LockFile::empty()),
        Err(e) => Err(LockError::Io(e).into()),
    }
}

/// Write the lock file atomically to the given root directory.
///
/// Keys are sorted deterministically for clean git diffs (IndexMap preserves
/// insertion order, so callers should ensure sorted order when building).
pub fn write(root: &Path, lock: &LockFile) -> Result<(), MarsError> {
    let path = root.join(LOCK_FILE);
    let content = toml::to_string_pretty(lock).map_err(|e| LockError::Corrupt {
        message: format!("failed to serialize lock file: {e}"),
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())
}

/// Build a new lock file from resolved graph + apply results.
///
/// Constructs the lock file from the graph (source provenance) and
/// the apply outcomes (checksums). Items that were skipped, kept, or
/// merged retain their provenance from the graph. Removed items are excluded.
pub fn build(
    graph: &crate::resolve::ResolvedGraph,
    applied: &crate::sync::apply::ApplyResult,
    old_lock: &LockFile,
) -> Result<LockFile, MarsError> {
    use crate::sync::apply::ActionTaken;

    let mut sources = IndexMap::new();
    let mut items = IndexMap::new();

    // Build source entries from graph
    for (name, node) in &graph.nodes {
        let resolved = &node.resolved_ref;
        let url = graph
            .nodes
            .get(name)
            .and_then(|n| {
                // If we have a version, it was a git source
                n.resolved_ref.commit.as_ref().map(|_| ())
            })
            .and_then(|_| {
                // Try to get URL from somewhere — it's not directly on ResolvedRef
                // We'll fill this from the old lock or leave it None
                old_lock.sources.get(name).and_then(|s| s.url.clone())
            });

        let path = if resolved.commit.is_none() && resolved.version.is_none() {
            // Path source
            Some(resolved.tree_path.to_string_lossy().to_string())
        } else {
            None
        };

        sources.insert(
            name.clone(),
            LockedSource {
                url,
                path,
                version: resolved.version_tag.clone(),
                commit: resolved.commit.clone(),
                tree_hash: None, // Could compute, but not critical for v1
            },
        );
    }

    // Build item entries from apply outcomes
    for outcome in &applied.outcomes {
        match &outcome.action {
            ActionTaken::Removed | ActionTaken::Skipped => {
                // For skipped items, carry forward from old lock
                if matches!(outcome.action, ActionTaken::Skipped) {
                    let dest_str = outcome.dest_path.to_string_lossy().to_string();
                    if let Some(old_item) = old_lock.items.get(&dest_str) {
                        items.insert(dest_str, old_item.clone());
                    }
                }
                // Removed items are excluded from the new lock
            }
            ActionTaken::Kept => {
                // Keep local: carry forward old lock entry (source unchanged)
                let dest_str = outcome.dest_path.to_string_lossy().to_string();
                if let Some(old_item) = old_lock.items.get(&dest_str) {
                    items.insert(dest_str, old_item.clone());
                }
            }
            ActionTaken::Installed
            | ActionTaken::Updated
            | ActionTaken::Merged
            | ActionTaken::Conflicted => {
                let dest_str = outcome.dest_path.to_string_lossy().to_string();
                if dest_str.is_empty() {
                    continue;
                }

                // Use source_name from outcome (propagated from TargetItem)
                let source_name = if outcome.source_name.is_empty() {
                    None
                } else {
                    Some(outcome.source_name.clone())
                };

                // Determine version from graph
                let version = source_name.as_ref().and_then(|sn| {
                    graph
                        .nodes
                        .get(sn)
                        .and_then(|n| n.resolved_ref.version_tag.clone())
                });

                let source_checksum = outcome
                    .source_checksum
                    .clone()
                    .unwrap_or_default();
                let installed_checksum = outcome
                    .installed_checksum
                    .clone()
                    .unwrap_or_else(|| source_checksum.clone());

                items.insert(
                    dest_str.clone(),
                    LockedItem {
                        source: source_name.unwrap_or_default(),
                        kind: outcome.item_id.kind,
                        version,
                        source_checksum,
                        installed_checksum,
                        dest_path: dest_str,
                    },
                );
            }
        }
    }

    // Sort items by key for deterministic output
    items.sort_keys();

    Ok(LockFile {
        version: 1,
        sources,
        items,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_lock() -> LockFile {
        let mut sources = IndexMap::new();
        sources.insert(
            "base".to_string(),
            LockedSource {
                url: Some("https://github.com/org/base.git".to_string()),
                path: None,
                version: Some("v1.0.0".to_string()),
                commit: Some("abc123".to_string()),
                tree_hash: Some("def456".to_string()),
            },
        );

        let mut items = IndexMap::new();
        items.insert(
            "agents/coder.md".to_string(),
            LockedItem {
                source: "base".to_string(),
                kind: ItemKind::Agent,
                version: Some("v1.0.0".to_string()),
                source_checksum: "sha256:aaa".to_string(),
                installed_checksum: "sha256:bbb".to_string(),
                dest_path: "agents/coder.md".to_string(),
            },
        );
        items.insert(
            "skills/review".to_string(),
            LockedItem {
                source: "base".to_string(),
                kind: ItemKind::Skill,
                version: Some("v1.0.0".to_string()),
                source_checksum: "sha256:ccc".to_string(),
                installed_checksum: "sha256:ddd".to_string(),
                dest_path: "skills/review".to_string(),
            },
        );

        LockFile {
            version: 1,
            sources,
            items,
        }
    }

    #[test]
    fn parse_valid_lock_file() {
        let toml_str = r#"
version = 1

[sources.base]
url = "https://github.com/org/base.git"
version = "v1.0.0"
commit = "abc123"
tree_hash = "def456"

[items."agents/coder.md"]
source = "base"
kind = "agent"
version = "v1.0.0"
source_checksum = "sha256:aaa"
installed_checksum = "sha256:bbb"
dest_path = "agents/coder.md"
"#;
        let lock: LockFile = toml::from_str(toml_str).unwrap();
        assert_eq!(lock.version, 1);
        assert_eq!(lock.sources.len(), 1);
        assert_eq!(lock.items.len(), 1);

        let item = &lock.items["agents/coder.md"];
        assert_eq!(item.source, "base");
        assert_eq!(item.kind, ItemKind::Agent);
        assert_eq!(item.source_checksum, "sha256:aaa");
        assert_eq!(item.installed_checksum, "sha256:bbb");
    }

    #[test]
    fn roundtrip_lock_file() {
        let lock = sample_lock();
        let serialized = toml::to_string_pretty(&lock).unwrap();
        let deserialized: LockFile = toml::from_str(&serialized).unwrap();
        assert_eq!(lock, deserialized);
    }

    #[test]
    fn deterministic_serialization() {
        let lock = sample_lock();
        let s1 = toml::to_string_pretty(&lock).unwrap();
        let s2 = toml::to_string_pretty(&lock).unwrap();
        assert_eq!(s1, s2);

        // Verify key ordering is preserved (agents/coder.md before skills/review)
        let coder_pos = s1.find("agents/coder.md").unwrap();
        let review_pos = s1.find("skills/review").unwrap();
        assert!(
            coder_pos < review_pos,
            "keys should preserve insertion order"
        );
    }

    #[test]
    fn empty_lock_file() {
        let lock = LockFile::empty();
        assert_eq!(lock.version, 1);
        assert!(lock.sources.is_empty());
        assert!(lock.items.is_empty());

        // Roundtrip empty
        let serialized = toml::to_string_pretty(&lock).unwrap();
        let deserialized: LockFile = toml::from_str(&serialized).unwrap();
        assert_eq!(lock, deserialized);
    }

    #[test]
    fn load_absent_returns_empty() {
        let dir = TempDir::new().unwrap();
        let lock = load(dir.path()).unwrap();
        assert_eq!(lock.version, 1);
        assert!(lock.sources.is_empty());
        assert!(lock.items.is_empty());
    }

    #[test]
    fn write_and_reload() {
        let dir = TempDir::new().unwrap();
        let lock = sample_lock();
        write(dir.path(), &lock).unwrap();
        let reloaded = load(dir.path()).unwrap();
        assert_eq!(lock, reloaded);
    }

    #[test]
    fn dual_checksums_present() {
        let lock = sample_lock();
        let item = &lock.items["agents/coder.md"];
        assert_ne!(item.source_checksum, item.installed_checksum);
        assert!(item.source_checksum.starts_with("sha256:"));
        assert!(item.installed_checksum.starts_with("sha256:"));
    }

    #[test]
    fn path_source_in_lock() {
        let toml_str = r#"
version = 1

[sources.local]
path = "/home/dev/agents"

[items."agents/helper.md"]
source = "local"
kind = "agent"
source_checksum = "sha256:111"
installed_checksum = "sha256:222"
dest_path = "agents/helper.md"
"#;
        let lock: LockFile = toml::from_str(toml_str).unwrap();
        let source = &lock.sources["local"];
        assert!(source.url.is_none());
        assert_eq!(source.path.as_deref(), Some("/home/dev/agents"));
        assert!(source.commit.is_none());
    }

    #[test]
    fn item_kind_serializes_lowercase() {
        let item = LockedItem {
            source: "base".to_string(),
            kind: ItemKind::Skill,
            version: None,
            source_checksum: "sha256:aaa".to_string(),
            installed_checksum: "sha256:bbb".to_string(),
            dest_path: "skills/review".to_string(),
        };
        let serialized = toml::to_string(&item).unwrap();
        assert!(serialized.contains("kind = \"skill\""));
    }

    #[test]
    fn item_id_display() {
        let id = ItemId {
            kind: ItemKind::Agent,
            name: "coder".to_string(),
        };
        assert_eq!(id.to_string(), "agent/coder");
    }

    #[test]
    fn item_kind_display() {
        assert_eq!(ItemKind::Agent.to_string(), "agent");
        assert_eq!(ItemKind::Skill.to_string(), "skill");
    }
}
