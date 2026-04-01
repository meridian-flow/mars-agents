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
/// Not yet implemented — depends on resolve and sync phases.
pub fn build(
    _graph: &crate::resolve::ResolvedGraph,
    _applied: &crate::sync::apply::ApplyResult,
    _pruned: &[crate::sync::apply::ActionOutcome],
) -> Result<LockFile, MarsError> {
    todo!()
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
