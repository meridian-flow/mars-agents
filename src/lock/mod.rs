use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{LockError, MarsError};
use crate::types::{
    CommitHash, ContentHash, DestPath, SourceId, SourceName, SourceOrigin, SourceUrl,
};

/// The complete lock file — ownership registry for all managed items.
///
/// Tracks every managed file with provenance and integrity data.
/// TOML format, deterministically ordered (sorted keys) for clean git diffs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockFile {
    /// Schema version, currently 1.
    pub version: u32,
    #[serde(default)]
    pub dependencies: IndexMap<SourceName, LockedSource>,
    #[serde(default)]
    pub items: IndexMap<DestPath, LockedItem>,
}

impl LockFile {
    /// Create a new empty lock file with the current schema version.
    pub fn empty() -> Self {
        LockFile {
            version: 1,
            dependencies: IndexMap::new(),
            items: IndexMap::new(),
        }
    }
}

/// One resolved source in the lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<SourceUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitHash>,
    /// Reserved for future content verification of fetched source trees.
    /// TODO: populate during fetch/build once deterministic tree hashing is implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_hash: Option<String>,
}

/// One installed item tracked by the lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedItem {
    pub source: SourceName,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub source_checksum: ContentHash,
    pub installed_checksum: ContentHash,
    pub dest_path: DestPath,
}

// Re-export ItemKind and ItemId from types — they're shared vocabulary,
// not lock-specific. This preserves `use crate::lock::ItemKind` compatibility.
pub use crate::types::{ItemId, ItemKind};

const LOCK_FILE: &str = "mars.lock";

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

    let mut dependencies = IndexMap::new();
    let mut items = IndexMap::new();

    // Build dependency entries directly from resolved graph provenance.
    for (name, node) in &graph.nodes {
        dependencies.insert(name.clone(), to_locked_source(node));
    }

    // Build item entries from apply outcomes
    for outcome in &applied.outcomes {
        match &outcome.action {
            ActionTaken::Removed | ActionTaken::Skipped => {
                // For skipped items, carry forward from old lock
                if matches!(outcome.action, ActionTaken::Skipped) {
                    let dest_path = outcome.dest_path.clone();
                    if let Some(old_item) = old_lock.items.get(&dest_path) {
                        items.insert(dest_path, old_item.clone());
                    }
                }
                // Removed items are excluded from the new lock
            }
            ActionTaken::Kept => {
                // Keep local: carry forward old lock entry (source unchanged)
                let dest_path = outcome.dest_path.clone();
                if let Some(old_item) = old_lock.items.get(&dest_path) {
                    items.insert(dest_path, old_item.clone());
                }
            }
            ActionTaken::Installed
            | ActionTaken::Updated
            | ActionTaken::Merged
            | ActionTaken::Conflicted => {
                let dest_path = outcome.dest_path.clone();
                if dest_path.as_path().as_os_str().is_empty() {
                    continue;
                }

                // Use source_name from outcome (propagated from TargetItem)
                let source_name = if outcome.source_name.as_ref().is_empty() {
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
                    .unwrap_or_else(|| ContentHash::from(""));
                let installed_checksum = outcome
                    .installed_checksum
                    .clone()
                    .unwrap_or_else(|| source_checksum.clone());

                items.insert(
                    dest_path.clone(),
                    LockedItem {
                        source: source_name.unwrap_or_else(|| SourceName::from("")),
                        kind: outcome.item_id.kind,
                        version,
                        source_checksum,
                        installed_checksum,
                        dest_path,
                    },
                );
            }
        }
    }

    // Add synthetic _self source if any local package items exist.
    let local_source_name: SourceName = SourceOrigin::LocalPackage.to_string().into();
    let has_self_items = items.values().any(|item| item.source == local_source_name);
    if has_self_items {
        dependencies.insert(
            local_source_name,
            LockedSource {
                url: None,
                path: Some(".".into()),
                version: None,
                commit: None,
                tree_hash: None,
            },
        );
    }

    // Sort keys for deterministic output.
    dependencies.sort_keys();
    items.sort_keys();

    Ok(LockFile {
        version: 1,
        dependencies,
        items,
    })
}

fn to_locked_source(node: &crate::resolve::ResolvedNode) -> LockedSource {
    let (url, path) = match &node.source_id {
        SourceId::Git { url } => (Some(url.clone()), None),
        SourceId::Path { canonical } => (None, Some(canonical.to_string_lossy().to_string())),
    };

    LockedSource {
        url,
        path,
        version: node.resolved_ref.version_tag.clone(),
        commit: node.resolved_ref.commit.clone(),
        tree_hash: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::resolve::{ResolvedGraph, ResolvedNode};
    use crate::source::ResolvedRef;
    use crate::sync::apply::{ActionOutcome, ActionTaken, ApplyResult};
    use crate::types::{SourceId, SourceUrl};
    use tempfile::TempDir;

    fn sample_lock() -> LockFile {
        let mut dependencies = IndexMap::new();
        dependencies.insert(
            "base".into(),
            LockedSource {
                url: Some("https://github.com/org/base.git".into()),
                path: None,
                version: Some("v1.0.0".into()),
                commit: Some("abc123".into()),
                tree_hash: Some("def456".into()),
            },
        );

        let mut items = IndexMap::new();
        items.insert(
            "agents/coder.md".into(),
            LockedItem {
                source: "base".into(),
                kind: ItemKind::Agent,
                version: Some("v1.0.0".into()),
                source_checksum: "sha256:aaa".into(),
                installed_checksum: "sha256:bbb".into(),
                dest_path: "agents/coder.md".into(),
            },
        );
        items.insert(
            "skills/review".into(),
            LockedItem {
                source: "base".into(),
                kind: ItemKind::Skill,
                version: Some("v1.0.0".into()),
                source_checksum: "sha256:ccc".into(),
                installed_checksum: "sha256:ddd".into(),
                dest_path: "skills/review".into(),
            },
        );

        LockFile {
            version: 1,
            dependencies,
            items,
        }
    }

    #[test]
    fn parse_valid_lock_file() {
        let toml_str = r#"
version = 1

[dependencies.base]
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
        assert_eq!(lock.dependencies.len(), 1);
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
        assert!(lock.dependencies.is_empty());
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
        assert!(lock.dependencies.is_empty());
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

[dependencies.local]
path = "/home/dev/agents"

[items."agents/helper.md"]
source = "local"
kind = "agent"
source_checksum = "sha256:111"
installed_checksum = "sha256:222"
dest_path = "agents/helper.md"
"#;
        let lock: LockFile = toml::from_str(toml_str).unwrap();
        let source = &lock.dependencies["local"];
        assert!(source.url.is_none());
        assert_eq!(source.path.as_deref(), Some("/home/dev/agents"));
        assert!(source.commit.is_none());
    }

    #[test]
    fn item_kind_serializes_lowercase() {
        let item = LockedItem {
            source: "base".into(),
            kind: ItemKind::Skill,
            version: None,
            source_checksum: "sha256:aaa".into(),
            installed_checksum: "sha256:bbb".into(),
            dest_path: "skills/review".into(),
        };
        let serialized = toml::to_string(&item).unwrap();
        assert!(serialized.contains("kind = \"skill\""));
    }

    #[test]
    fn item_id_display() {
        let id = ItemId {
            kind: ItemKind::Agent,
            name: "coder".into(),
        };
        assert_eq!(id.to_string(), "agent/coder");
    }

    #[test]
    fn item_kind_display() {
        assert_eq!(ItemKind::Agent.to_string(), "agent");
        assert_eq!(ItemKind::Skill.to_string(), "skill");
    }

    #[test]
    fn build_uses_graph_provenance_for_sources() {
        let git_name: SourceName = "base".into();
        let path_name: SourceName = "local".into();
        let git_url: SourceUrl = "https://example.com/new.git".into();
        let path_canonical = PathBuf::from("/tmp/mars-agents-local-source");

        let mut nodes = IndexMap::new();
        nodes.insert(
            git_name.clone(),
            ResolvedNode {
                source_name: git_name.clone(),
                source_id: SourceId::git(git_url.clone()),
                resolved_ref: ResolvedRef {
                    source_name: git_name.clone(),
                    version: Some(semver::Version::new(1, 2, 3)),
                    version_tag: Some("v1.2.3".into()),
                    commit: Some("abc123".into()),
                    tree_path: PathBuf::from("/tmp/cache/base"),
                },
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );
        nodes.insert(
            path_name.clone(),
            ResolvedNode {
                source_name: path_name.clone(),
                source_id: SourceId::Path {
                    canonical: path_canonical.clone(),
                },
                resolved_ref: ResolvedRef {
                    source_name: path_name.clone(),
                    version: None,
                    version_tag: None,
                    commit: None,
                    tree_path: PathBuf::from("/tmp/cache/local"),
                },
                latest_version: None,
                manifest: None,
                deps: vec![],
            },
        );

        let graph = ResolvedGraph {
            nodes,
            order: vec![git_name.clone(), path_name.clone()],
            id_index: HashMap::new(),
            filters: HashMap::new(),
        };
        let applied = ApplyResult { outcomes: vec![] };

        let mut old_sources = IndexMap::new();
        old_sources.insert(
            git_name.clone(),
            LockedSource {
                url: Some("https://example.com/old.git".into()),
                path: None,
                version: Some("v0.0.1".into()),
                commit: Some("deadbeef".into()),
                tree_hash: None,
            },
        );
        let old_lock = LockFile {
            version: 1,
            dependencies: old_sources,
            items: IndexMap::new(),
        };

        let new_lock = build(&graph, &applied, &old_lock).unwrap();

        let base = &new_lock.dependencies["base"];
        assert_eq!(base.url.as_ref(), Some(&git_url));
        assert_eq!(base.version.as_deref(), Some("v1.2.3"));
        assert_eq!(base.commit.as_deref(), Some("abc123"));

        let local = &new_lock.dependencies["local"];
        assert!(local.url.is_none());
        assert_eq!(
            local.path.as_deref(),
            Some(path_canonical.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn build_keeps_self_items_from_old_lock_on_skipped_action() {
        let graph = ResolvedGraph {
            nodes: IndexMap::new(),
            order: Vec::new(),
            id_index: HashMap::new(),
            filters: HashMap::new(),
        };
        let local_source_name: SourceName = SourceOrigin::LocalPackage.to_string().into();
        let old_lock = LockFile {
            version: 1,
            dependencies: IndexMap::from([(
                local_source_name.clone(),
                LockedSource {
                    url: None,
                    path: Some(".".into()),
                    version: None,
                    commit: None,
                    tree_hash: None,
                },
            )]),
            items: IndexMap::from([(
                DestPath::from("skills/local-skill"),
                LockedItem {
                    source: local_source_name.clone(),
                    kind: ItemKind::Skill,
                    version: None,
                    source_checksum: "sha256:self".into(),
                    installed_checksum: "sha256:self".into(),
                    dest_path: DestPath::from("skills/local-skill"),
                },
            )]),
        };
        let applied = ApplyResult {
            outcomes: vec![ActionOutcome {
                item_id: ItemId {
                    kind: ItemKind::Skill,
                    name: "local-skill".into(),
                },
                action: ActionTaken::Skipped,
                dest_path: "skills/local-skill".into(),
                source_name: local_source_name.clone(),
                source_checksum: None,
                installed_checksum: None,
            }],
        };

        let new_lock = build(&graph, &applied, &old_lock).unwrap();

        assert!(
            new_lock
                .dependencies
                .contains_key(local_source_name.as_str())
        );
        let item = &new_lock.items["skills/local-skill"];
        assert_eq!(item.source, local_source_name);
        assert_eq!(item.kind, ItemKind::Skill);
        assert_eq!(item.source_checksum, "sha256:self");
        assert_eq!(item.installed_checksum, "sha256:self");
    }
}
