use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::diagnostic::Diagnostic;
use crate::error::{LockError, MarsError};
use crate::types::{
    CommitHash, ContentHash, DestPath, SourceId, SourceName, SourceOrigin, SourceSubpath, SourceUrl,
};

/// The complete lock file — ownership registry for all managed items.
///
/// Schema version 2: items are keyed by logical identity ("kind/name"), and each item
/// carries a list of per-output records (one per target root materialization).
///
/// TOML format, deterministically ordered (sorted keys) for clean git diffs.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LockFile {
    /// Schema version. Current version is 2.
    pub version: u32,
    #[serde(default)]
    pub dependencies: IndexMap<SourceName, LockedSource>,
    /// V2: logical items keyed by "kind/name" identity string.
    #[serde(default)]
    pub items: IndexMap<String, LockedItemV2>,
}

/// Custom `Deserialize` for `LockFile`: delegates to the v2 wire type.
///
/// For reading v1 lock files, always go through [`load()`] which handles
/// the v1→v2 promotion. Direct deserialization via `toml::from_str::<LockFile>`
/// only supports v2 format.
impl<'de> serde::Deserialize<'de> for LockFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = LockFileV2Wire::deserialize(deserializer)?;
        Ok(LockFile {
            version: wire.version,
            dependencies: wire.dependencies,
            items: wire.items,
        })
    }
}

impl LockFile {
    /// Create a new empty lock file with the current schema version.
    pub fn empty() -> Self {
        LockFile {
            version: LOCK_VERSION,
            dependencies: IndexMap::new(),
            items: IndexMap::new(),
        }
    }

    /// Look up a locked item by its output dest_path, returning a flat [`LockedItem`] view.
    ///
    /// Searches across all items and their output records. Returns the first match.
    pub fn find_by_dest_path(&self, dest_path: &DestPath) -> Option<LockedItem> {
        for item_v2 in self.items.values() {
            for output in &item_v2.outputs {
                if &output.dest_path == dest_path {
                    return Some(LockedItem {
                        source: item_v2.source.clone(),
                        kind: item_v2.kind,
                        version: item_v2.version.clone(),
                        source_checksum: item_v2.source_checksum.clone(),
                        installed_checksum: output.installed_checksum.clone(),
                        dest_path: output.dest_path.clone(),
                    });
                }
            }
        }
        None
    }

    /// Check if any output record has the given dest_path.
    pub fn contains_dest_path(&self, dest_path: &DestPath) -> bool {
        self.items
            .values()
            .any(|item| item.outputs.iter().any(|o| &o.dest_path == dest_path))
    }

    /// Iterate all output dest_paths across all items.
    pub fn all_output_dest_paths(&self) -> impl Iterator<Item = &DestPath> {
        self.items
            .values()
            .flat_map(|item| item.outputs.iter().map(|o| &o.dest_path))
    }

    /// Flat view of all items as owned `(dest_path, LockedItem)` pairs.
    ///
    /// Used by diff, orphan scan, and CLI commands that need a per-output view.
    pub fn flat_items(&self) -> Vec<(DestPath, LockedItem)> {
        self.items
            .values()
            .flat_map(|item_v2| {
                item_v2.outputs.iter().map(|output| {
                    (
                        output.dest_path.clone(),
                        LockedItem {
                            source: item_v2.source.clone(),
                            kind: item_v2.kind,
                            version: item_v2.version.clone(),
                            source_checksum: item_v2.source_checksum.clone(),
                            installed_checksum: output.installed_checksum.clone(),
                            dest_path: output.dest_path.clone(),
                        },
                    )
                })
            })
            .collect()
    }
}

/// Ephemeral lookup index for lock files.
///
/// `LockFile` preserves the persisted v2 shape. Build this short-lived index
/// at hot call sites that need repeated output-path lookups.
pub struct LockIndex<'a> {
    lock: &'a LockFile,
    by_dest_path: HashMap<&'a DestPath, (&'a str, usize)>,
}

impl<'a> LockIndex<'a> {
    pub fn new(lock: &'a LockFile) -> Self {
        let by_dest_path = lock
            .items
            .iter()
            .flat_map(|(key, item)| {
                item.outputs
                    .iter()
                    .enumerate()
                    .map(move |(idx, output)| (&output.dest_path, (key.as_str(), idx)))
            })
            .collect();

        Self { lock, by_dest_path }
    }

    /// Look up a locked item by output dest_path, returning a flat [`LockedItem`] view.
    pub fn find_by_dest_path(&self, dest_path: &DestPath) -> Option<LockedItem> {
        let (item_key, output_idx) = self.by_dest_path.get(dest_path)?;
        let item_v2 = self.lock.items.get(*item_key)?;
        let output = item_v2.outputs.get(*output_idx)?;
        Some(LockedItem {
            source: item_v2.source.clone(),
            kind: item_v2.kind,
            version: item_v2.version.clone(),
            source_checksum: item_v2.source_checksum.clone(),
            installed_checksum: output.installed_checksum.clone(),
            dest_path: output.dest_path.clone(),
        })
    }

    /// Check if any output record has the given dest_path.
    pub fn contains_dest_path(&self, dest_path: &DestPath) -> bool {
        self.by_dest_path.contains_key(dest_path)
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
    pub subpath: Option<SourceSubpath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitHash>,
    /// Reserved for future content verification of fetched source trees.
    /// TODO: populate during fetch/build once deterministic tree hashing is implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_hash: Option<String>,
}

/// V2 locked item: one logical item with per-output records.
///
/// `source_checksum` is shared across all outputs (same source content).
/// Each `OutputRecord` has its own `installed_checksum` for divergence detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedItemV2 {
    pub source: SourceName,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub source_checksum: ContentHash,
    /// Per-output records: one per target root this item was materialized to.
    pub outputs: Vec<OutputRecord>,
}

/// A single materialized output of a logical item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputRecord {
    /// Target root this output belongs to (e.g., ".mars", ".agents", ".claude").
    pub target_root: String,
    /// Relative path under the target root (e.g., "agents/coder.md").
    pub dest_path: DestPath,
    /// Checksum of the installed content at this output location.
    pub installed_checksum: ContentHash,
}

/// Flat view of a single installed item — used by diff, plan, and apply stages.
///
/// Constructed from [`LockedItemV2`] + one [`OutputRecord`]; preserves backward
/// compat with code that operates on per-dest-path records.
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
/// Current lock file schema version.
const LOCK_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// V1 wire type — used only for reading legacy lock files.
// ---------------------------------------------------------------------------

/// V1 wire format for reading legacy lock files.
#[derive(Deserialize)]
struct LockFileV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    dependencies: IndexMap<SourceName, LockedSource>,
    #[serde(default)]
    items: IndexMap<DestPath, LockedItem>,
}

/// V2 wire format for Deserialize (mirrors `LockFile` but derives `Deserialize`).
#[derive(Deserialize)]
struct LockFileV2Wire {
    version: u32,
    #[serde(default)]
    dependencies: IndexMap<SourceName, LockedSource>,
    #[serde(default)]
    items: IndexMap<String, LockedItemV2>,
}

// ---------------------------------------------------------------------------
// Load / write
// ---------------------------------------------------------------------------

/// Load the lock file from the given root directory.
///
/// Returns an empty LockFile (v2) if the file is absent.
/// V1 lock files are transparently promoted to the v2 in-memory shape (D19):
/// the lock is only written as v2 after a successful sync.
pub fn load(root: &Path) -> Result<LockFile, MarsError> {
    let (lock, _) = load_with_diagnostics(root)?;
    Ok(lock)
}

/// Load the lock file and return any diagnostics produced while reading it.
///
/// This preserves legacy v1→v2 in-memory promotion while routing promotion
/// warnings through the normal diagnostic flow for sync callers.
pub fn load_with_diagnostics(root: &Path) -> Result<(LockFile, Vec<Diagnostic>), MarsError> {
    let path = root.join(LOCK_FILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((LockFile::empty(), Vec::new()));
        }
        Err(e) => return Err(LockError::Io(e).into()),
    };

    let value: toml::Value = toml::from_str(&content).map_err(|e| LockError::Corrupt {
        message: format!("failed to parse {}: {e}", path.display()),
    })?;

    match value.clone().try_into::<LockFileV2Wire>() {
        Ok(wire) if wire.version >= 2 => Ok((
            LockFile {
                version: wire.version,
                dependencies: wire.dependencies,
                items: wire.items,
            },
            Vec::new(),
        )),
        v2_result => {
            // V1 → V2 promotion (D19): map each DestPath key to a logical identity.
            let wire: LockFileV1 = value.clone().try_into().map_err(|v1_error| {
                let parse_error = match v2_result {
                    Ok(wire) => format!("unsupported lock version {}", wire.version),
                    Err(v2_error) => {
                        format!("v2 parse failed: {v2_error}; v1 parse failed: {v1_error}")
                    }
                };
                LockError::Corrupt {
                    message: format!("failed to parse {}: {parse_error}", path.display()),
                }
            })?;
            let (items, diagnostics) = promote_v1_items(wire.items);
            Ok((
                LockFile {
                    version: LOCK_VERSION,
                    dependencies: wire.dependencies,
                    items,
                },
                diagnostics,
            ))
        }
    }
}

/// Write the lock file atomically to the given root directory (always v2 format).
pub fn write(root: &Path, lock: &LockFile) -> Result<(), MarsError> {
    let path = root.join(LOCK_FILE);
    let content = toml::to_string_pretty(lock).map_err(|e| LockError::Corrupt {
        message: format!("failed to serialize lock file: {e}"),
    })?;
    crate::fs::atomic_write(&path, content.as_bytes())
}

/// Convert v1 `IndexMap<DestPath, LockedItem>` to v2 `IndexMap<String, LockedItemV2>`.
///
/// Each v1 entry becomes one `LockedItemV2` with exactly one `OutputRecord`
/// using `target_root = ".mars"` (the only output root in v1).
///
/// Key collision: two v1 entries with different dest_paths but the same basename
/// (e.g. `hooks/pre-commit/hook.sh` and `hooks/pre-push/hook.sh` both name "hook")
/// would map to the same key and silently drop one. When a collision is detected,
/// we warn and fall back to the raw dest_path string as a disambiguated key.
fn promote_v1_items(
    v1_items: IndexMap<DestPath, LockedItem>,
) -> (IndexMap<String, LockedItemV2>, Vec<Diagnostic>) {
    let mut result: IndexMap<String, LockedItemV2> = IndexMap::new();
    let mut diagnostics = Vec::new();

    for (dest_path, item) in v1_items {
        let key = format!("{}/{}", item.kind, dest_path.item_name(item.kind));
        let item_v2 = LockedItemV2 {
            source: item.source,
            kind: item.kind,
            version: item.version,
            source_checksum: item.source_checksum,
            outputs: vec![OutputRecord {
                target_root: ".mars".to_string(),
                dest_path: item.dest_path,
                installed_checksum: item.installed_checksum,
            }],
        };

        if result.contains_key(&key) {
            // Two v1 entries share the same basename — use the full dest_path as a
            // disambiguated key so neither entry is silently dropped.
            let fallback_key = format!("{}/{}", item_v2.kind, dest_path.as_str());
            diagnostics.push(Diagnostic {
                level: crate::diagnostic::DiagnosticLevel::Warning,
                code: "lock-promotion-collision",
                message: format!(
                    "v1→v2 promotion: key collision on `{key}`; using dest_path key `{fallback_key}`"
                ),
                context: None,
                category: None,
            });
            result.insert(fallback_key, item_v2);
        } else {
            result.insert(key, item_v2);
        }
    }

    (result, diagnostics)
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

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
    let mut items: IndexMap<String, LockedItemV2> = IndexMap::new();
    let old_lock_index = LockIndex::new(old_lock);

    for outcome in &applied.outcomes {
        match outcome.action {
            ActionTaken::Installed
            | ActionTaken::Updated
            | ActionTaken::Merged
            | ActionTaken::Conflicted => {
                let installed =
                    outcome
                        .installed_checksum
                        .as_ref()
                        .ok_or_else(|| LockError::Corrupt {
                            message: format!(
                                "missing checksum for write-producing action on {}",
                                outcome.dest_path
                            ),
                        })?;
                if checksum_is_empty(installed) {
                    return Err(LockError::Corrupt {
                        message: format!("empty installed_checksum for {}", outcome.dest_path),
                    }
                    .into());
                }

                let source =
                    outcome
                        .source_checksum
                        .as_ref()
                        .ok_or_else(|| LockError::Corrupt {
                            message: format!(
                                "missing source checksum for write-producing action on {}",
                                outcome.dest_path
                            ),
                        })?;
                if checksum_is_empty(source) {
                    return Err(LockError::Corrupt {
                        message: format!("empty source_checksum for {}", outcome.dest_path),
                    }
                    .into());
                }
            }
            ActionTaken::Removed | ActionTaken::Skipped | ActionTaken::Kept => {}
        }
    }

    // Build dependency entries directly from resolved graph provenance.
    for (name, node) in &graph.nodes {
        dependencies.insert(name.clone(), to_locked_source(node));
    }

    // Build item entries from apply outcomes.
    for outcome in &applied.outcomes {
        match &outcome.action {
            ActionTaken::Removed | ActionTaken::Skipped => {
                // For skipped items, carry forward from old lock
                if matches!(outcome.action, ActionTaken::Skipped) {
                    let item_key = item_key(&outcome.item_id);
                    if let Some(old_item) = old_lock.items.get(&item_key) {
                        items.insert(item_key, old_item.clone());
                    } else {
                        // Fall back: search old lock by dest_path (handles v1→v2 migrations
                        // where item_key may not match yet)
                        if let Some(flat) = old_lock_index.find_by_dest_path(&outcome.dest_path) {
                            let key =
                                format!("{}/{}", flat.kind, outcome.dest_path.item_name(flat.kind));
                            items.entry(key).or_insert_with(|| LockedItemV2 {
                                source: flat.source,
                                kind: flat.kind,
                                version: flat.version,
                                source_checksum: flat.source_checksum,
                                outputs: vec![OutputRecord {
                                    target_root: ".mars".to_string(),
                                    dest_path: flat.dest_path,
                                    installed_checksum: flat.installed_checksum,
                                }],
                            });
                        }
                    }
                }
                // Removed items are excluded from the new lock.
            }
            ActionTaken::Kept => {
                // Keep local: carry forward old lock entry.
                let item_key = item_key(&outcome.item_id);
                if let Some(old_item) = old_lock.items.get(&item_key) {
                    items.insert(item_key, old_item.clone());
                } else if let Some(flat) = old_lock_index.find_by_dest_path(&outcome.dest_path) {
                    let key = format!("{}/{}", flat.kind, outcome.dest_path.item_name(flat.kind));
                    items.entry(key).or_insert_with(|| LockedItemV2 {
                        source: flat.source,
                        kind: flat.kind,
                        version: flat.version,
                        source_checksum: flat.source_checksum,
                        outputs: vec![OutputRecord {
                            target_root: ".mars".to_string(),
                            dest_path: flat.dest_path,
                            installed_checksum: flat.installed_checksum,
                        }],
                    });
                }
            }
            ActionTaken::Installed
            | ActionTaken::Updated
            | ActionTaken::Merged
            | ActionTaken::Conflicted => {
                let dest_path = outcome.dest_path.clone();
                if dest_path.as_str().is_empty() {
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
                    .expect("validated above: source_checksum exists for write actions");
                let installed_checksum = outcome
                    .installed_checksum
                    .clone()
                    .expect("validated above: installed_checksum exists for write actions");

                let key = item_key(&outcome.item_id);
                items.insert(
                    key,
                    LockedItemV2 {
                        source: source_name.unwrap_or_else(|| SourceName::from("")),
                        kind: outcome.item_id.kind,
                        version,
                        source_checksum,
                        outputs: vec![OutputRecord {
                            target_root: ".mars".to_string(),
                            dest_path,
                            installed_checksum,
                        }],
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
                subpath: None,
                version: None,
                commit: None,
                tree_hash: None,
            },
        );
    }

    // Validate checksums.
    for item in items.values() {
        if checksum_is_empty(&item.source_checksum) {
            let dest = item
                .outputs
                .first()
                .map(|o| o.dest_path.to_string())
                .unwrap_or_default();
            return Err(LockError::Corrupt {
                message: format!("empty source_checksum for {dest}"),
            }
            .into());
        }
        for output in &item.outputs {
            if checksum_is_empty(&output.installed_checksum) {
                return Err(LockError::Corrupt {
                    message: format!("empty installed_checksum for {}", output.dest_path),
                }
                .into());
            }
        }
    }

    // Sort keys for deterministic output.
    dependencies.sort_keys();
    items.sort_keys();

    Ok(LockFile {
        version: LOCK_VERSION,
        dependencies,
        items,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn checksum_is_empty(checksum: &ContentHash) -> bool {
    checksum.as_ref().trim().is_empty()
}

fn to_locked_source(node: &crate::resolve::ResolvedNode) -> LockedSource {
    let (url, path, subpath) = match &node.source_id {
        SourceId::Git { url, subpath } => (Some(url.clone()), None, subpath.clone()),
        SourceId::Path { canonical, subpath } => (
            None,
            Some(canonical.to_string_lossy().to_string()),
            subpath.clone(),
        ),
    };

    LockedSource {
        url,
        path,
        subpath,
        version: node.resolved_ref.version_tag.clone(),
        commit: node.resolved_ref.commit.clone(),
        tree_hash: None,
    }
}

/// Canonical item key for v2 lock: `"kind/name"`.
pub fn item_key(id: &ItemId) -> String {
    format!("{}/{}", id.kind, id.name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
                subpath: None,
                version: Some("v1.0.0".into()),
                commit: Some("abc123".into()),
                tree_hash: Some("def456".into()),
            },
        );

        let mut items = IndexMap::new();
        items.insert(
            "agent/coder".to_string(),
            LockedItemV2 {
                source: "base".into(),
                kind: ItemKind::Agent,
                version: Some("v1.0.0".into()),
                source_checksum: "sha256:aaa".into(),
                outputs: vec![OutputRecord {
                    target_root: ".mars".to_string(),
                    dest_path: "agents/coder.md".into(),
                    installed_checksum: "sha256:bbb".into(),
                }],
            },
        );
        items.insert(
            "skill/review".to_string(),
            LockedItemV2 {
                source: "base".into(),
                kind: ItemKind::Skill,
                version: Some("v1.0.0".into()),
                source_checksum: "sha256:ccc".into(),
                outputs: vec![OutputRecord {
                    target_root: ".mars".to_string(),
                    dest_path: "skills/review".into(),
                    installed_checksum: "sha256:ddd".into(),
                }],
            },
        );

        LockFile {
            version: LOCK_VERSION,
            dependencies,
            items,
        }
    }

    #[test]
    fn parse_v1_lock_file_promoted_to_v2() {
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
        // Load via the full load() path (promotion happens there).
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.lock"), toml_str).unwrap();
        let lock = load(dir.path()).unwrap();

        // Promoted to v2 in memory.
        assert_eq!(lock.version, LOCK_VERSION);
        assert_eq!(lock.dependencies.len(), 1);
        assert_eq!(lock.items.len(), 1);

        // V2 key is "kind/name".
        let item = &lock.items["agent/coder"];
        assert_eq!(item.source, "base");
        assert_eq!(item.kind, ItemKind::Agent);
        assert_eq!(item.source_checksum, "sha256:aaa");
        assert_eq!(item.outputs.len(), 1);
        assert_eq!(item.outputs[0].installed_checksum, "sha256:bbb");
        assert_eq!(item.outputs[0].dest_path.as_str(), "agents/coder.md");
        assert_eq!(item.outputs[0].target_root, ".mars");
    }

    #[test]
    fn parse_v2_lock_file() {
        let toml_str = r#"
version = 2

[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0.0"
commit = "abc123"

[items."agent/coder"]
source = "base"
kind = "agent"
version = "v1.0.0"
source_checksum = "sha256:aaa"

[[items."agent/coder".outputs]]
target_root = ".mars"
dest_path = "agents/coder.md"
installed_checksum = "sha256:bbb"
"#;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.lock"), toml_str).unwrap();
        let lock = load(dir.path()).unwrap();

        assert_eq!(lock.version, 2);
        assert_eq!(lock.items.len(), 1);

        let item = &lock.items["agent/coder"];
        assert_eq!(item.source_checksum, "sha256:aaa");
        assert_eq!(item.outputs[0].installed_checksum, "sha256:bbb");
    }

    #[test]
    fn roundtrip_lock_file() {
        let lock = sample_lock();
        let dir = TempDir::new().unwrap();
        write(dir.path(), &lock).unwrap();
        let reloaded = load(dir.path()).unwrap();
        assert_eq!(lock, reloaded);
    }

    #[test]
    fn deterministic_serialization() {
        let lock = sample_lock();
        let s1 = toml::to_string_pretty(&lock).unwrap();
        let s2 = toml::to_string_pretty(&lock).unwrap();
        assert_eq!(s1, s2);

        // V2: keys are "agent/coder" and "skill/review" — agent comes before skill alphabetically.
        let coder_pos = s1.find("agent/coder").unwrap();
        let review_pos = s1.find("skill/review").unwrap();
        assert!(
            coder_pos < review_pos,
            "agent/coder should appear before skill/review"
        );
    }

    #[test]
    fn empty_lock_file() {
        let lock = LockFile::empty();
        assert_eq!(lock.version, LOCK_VERSION);
        assert!(lock.dependencies.is_empty());
        assert!(lock.items.is_empty());
    }

    #[test]
    fn load_absent_returns_empty() {
        let dir = TempDir::new().unwrap();
        let lock = load(dir.path()).unwrap();
        assert_eq!(lock.version, LOCK_VERSION);
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
        let item = &lock.items["agent/coder"];
        assert_ne!(item.source_checksum, item.outputs[0].installed_checksum);
        assert!(item.source_checksum.starts_with("sha256:"));
        assert!(item.outputs[0].installed_checksum.starts_with("sha256:"));
    }

    #[test]
    fn path_source_in_lock() {
        let toml_str = r#"
version = 2

[dependencies.local]
path = "/home/dev/agents"

[items."agent/helper"]
source = "local"
kind = "agent"
source_checksum = "sha256:111"

[[items."agent/helper".outputs]]
target_root = ".mars"
dest_path = "agents/helper.md"
installed_checksum = "sha256:222"
"#;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.lock"), toml_str).unwrap();
        let lock = load(dir.path()).unwrap();
        let source = &lock.dependencies["local"];
        assert!(source.url.is_none());
        assert_eq!(source.path.as_deref(), Some("/home/dev/agents"));
        assert!(source.commit.is_none());
    }

    #[test]
    fn item_kind_serializes_lowercase() {
        let item = LockedItemV2 {
            source: "base".into(),
            kind: ItemKind::Skill,
            version: None,
            source_checksum: "sha256:aaa".into(),
            outputs: vec![OutputRecord {
                target_root: ".mars".to_string(),
                dest_path: "skills/review".into(),
                installed_checksum: "sha256:bbb".into(),
            }],
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
    fn find_by_dest_path_returns_flat_view() {
        let lock = sample_lock();
        let found = lock
            .find_by_dest_path(&DestPath::from("agents/coder.md"))
            .unwrap();
        assert_eq!(found.source, "base");
        assert_eq!(found.kind, ItemKind::Agent);
        assert_eq!(found.source_checksum, "sha256:aaa");
        assert_eq!(found.installed_checksum, "sha256:bbb");
        assert_eq!(found.dest_path.as_str(), "agents/coder.md");
    }

    #[test]
    fn find_by_dest_path_missing_returns_none() {
        let lock = sample_lock();
        assert!(
            lock.find_by_dest_path(&DestPath::from("agents/missing.md"))
                .is_none()
        );
    }

    #[test]
    fn contains_dest_path_hit_and_miss() {
        let lock = sample_lock();
        assert!(lock.contains_dest_path(&DestPath::from("agents/coder.md")));
        assert!(!lock.contains_dest_path(&DestPath::from("agents/nobody.md")));
    }

    #[test]
    fn lock_index_find_by_dest_path_hit_and_miss() {
        let lock = sample_lock();
        let index = LockIndex::new(&lock);

        let found = index
            .find_by_dest_path(&DestPath::from("agents/coder.md"))
            .unwrap();
        assert_eq!(found.source, "base");
        assert_eq!(found.kind, ItemKind::Agent);
        assert_eq!(found.source_checksum, "sha256:aaa");
        assert_eq!(found.installed_checksum, "sha256:bbb");
        assert_eq!(found.dest_path.as_str(), "agents/coder.md");

        assert!(
            index
                .find_by_dest_path(&DestPath::from("agents/missing.md"))
                .is_none()
        );
    }

    #[test]
    fn lock_index_contains_dest_path_hit_and_miss() {
        let lock = sample_lock();
        let index = LockIndex::new(&lock);

        assert!(index.contains_dest_path(&DestPath::from("agents/coder.md")));
        assert!(!index.contains_dest_path(&DestPath::from("agents/nobody.md")));
    }

    #[test]
    fn flat_items_yields_all_outputs() {
        let lock = sample_lock();
        let flat = lock.flat_items();
        assert_eq!(flat.len(), 2);
        let paths: Vec<&str> = flat.iter().map(|(dp, _)| dp.as_str()).collect();
        assert!(paths.contains(&"agents/coder.md"));
        assert!(paths.contains(&"skills/review"));
    }

    #[test]
    fn v1_lock_no_spurious_reinstall() {
        // V1 lock loaded → promoted to v2 → find_by_dest_path works for diff.
        let v1_toml = r#"
version = 1

[dependencies.base]
url = "https://github.com/org/base.git"

[items."agents/coder.md"]
source = "base"
kind = "agent"
source_checksum = "sha256:src"
installed_checksum = "sha256:inst"
dest_path = "agents/coder.md"
"#;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.lock"), v1_toml).unwrap();
        let lock = load(dir.path()).unwrap();

        // Promoted items should still be findable by dest_path.
        let found = lock.find_by_dest_path(&DestPath::from("agents/coder.md"));
        assert!(found.is_some());
        let item = found.unwrap();
        assert_eq!(item.source_checksum, "sha256:src");
        assert_eq!(item.installed_checksum, "sha256:inst");
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
                source_id: SourceId::git_with_subpath(
                    git_url.clone(),
                    Some(crate::types::SourceSubpath::new("plugins/base").unwrap()),
                ),
                rooted_ref: crate::resolve::RootedSourceRef {
                    checkout_root: PathBuf::from("/tmp/cache/base"),
                    package_root: PathBuf::from("/tmp/cache/base/plugins/base"),
                },
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
                    subpath: Some(crate::types::SourceSubpath::new("plugins/local").unwrap()),
                },
                rooted_ref: crate::resolve::RootedSourceRef {
                    checkout_root: PathBuf::from("/tmp/cache/local"),
                    package_root: PathBuf::from("/tmp/cache/local/plugins/local"),
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
            filters: HashMap::new(),
        };
        let applied = ApplyResult { outcomes: vec![] };

        let mut old_sources = IndexMap::new();
        old_sources.insert(
            git_name.clone(),
            LockedSource {
                url: Some("https://example.com/old.git".into()),
                path: None,
                subpath: None,
                version: Some("v0.0.1".into()),
                commit: Some("deadbeef".into()),
                tree_hash: None,
            },
        );
        let old_lock = LockFile {
            version: LOCK_VERSION,
            dependencies: old_sources,
            items: IndexMap::new(),
        };

        let new_lock = build(&graph, &applied, &old_lock).unwrap();

        let base = &new_lock.dependencies["base"];
        assert_eq!(base.url.as_ref(), Some(&git_url));
        assert_eq!(
            base.subpath
                .as_ref()
                .map(crate::types::SourceSubpath::as_str),
            Some("plugins/base")
        );
        assert_eq!(base.version.as_deref(), Some("v1.2.3"));
        assert_eq!(base.commit.as_deref(), Some("abc123"));

        let local = &new_lock.dependencies["local"];
        assert!(local.url.is_none());
        assert_eq!(
            local
                .subpath
                .as_ref()
                .map(crate::types::SourceSubpath::as_str),
            Some("plugins/local")
        );
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
            filters: HashMap::new(),
        };
        let local_source_name: SourceName = SourceOrigin::LocalPackage.to_string().into();
        let old_lock = LockFile {
            version: LOCK_VERSION,
            dependencies: IndexMap::from([(
                local_source_name.clone(),
                LockedSource {
                    url: None,
                    path: Some(".".into()),
                    subpath: None,
                    version: None,
                    commit: None,
                    tree_hash: None,
                },
            )]),
            items: IndexMap::from([(
                "skill/local-skill".to_string(),
                LockedItemV2 {
                    source: local_source_name.clone(),
                    kind: ItemKind::Skill,
                    version: None,
                    source_checksum: "sha256:self".into(),
                    outputs: vec![OutputRecord {
                        target_root: ".mars".to_string(),
                        dest_path: DestPath::from("skills/local-skill"),
                        installed_checksum: "sha256:self".into(),
                    }],
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
        let item = &new_lock.items["skill/local-skill"];
        assert_eq!(item.source, local_source_name);
        assert_eq!(item.kind, ItemKind::Skill);
        assert_eq!(item.source_checksum, "sha256:self");
        assert_eq!(item.outputs[0].installed_checksum, "sha256:self");
    }

    #[test]
    fn build_rejects_missing_installed_checksum_for_write_actions() {
        let graph = ResolvedGraph {
            nodes: IndexMap::new(),
            order: Vec::new(),
            filters: HashMap::new(),
        };
        let old_lock = LockFile::empty();
        let applied = ApplyResult {
            outcomes: vec![ActionOutcome {
                item_id: ItemId {
                    kind: ItemKind::Agent,
                    name: "coder".into(),
                },
                action: ActionTaken::Installed,
                dest_path: "agents/coder.md".into(),
                source_name: "base".into(),
                source_checksum: Some("sha256:source".into()),
                installed_checksum: None,
            }],
        };

        let err = build(&graph, &applied, &old_lock).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing checksum for write-producing action"));
        assert!(msg.contains("agents/coder.md"));
    }

    #[test]
    fn promote_v1_collision_both_survive() {
        // Two v1 items with different full dest_paths but the same basename
        // (e.g. "hook" from two different subdirectories) must both survive promotion.
        // Without collision handling the second would silently overwrite the first.
        let mut v1_items: IndexMap<DestPath, LockedItem> = IndexMap::new();

        v1_items.insert(
            DestPath::from("hooks/pre-commit/hook.sh"),
            LockedItem {
                source: "base".into(),
                kind: ItemKind::Hook,
                version: None,
                source_checksum: "sha256:aaa".into(),
                installed_checksum: "sha256:bbb".into(),
                dest_path: DestPath::from("hooks/pre-commit/hook.sh"),
            },
        );
        v1_items.insert(
            DestPath::from("hooks/pre-push/hook.sh"),
            LockedItem {
                source: "base".into(),
                kind: ItemKind::Hook,
                version: None,
                source_checksum: "sha256:ccc".into(),
                installed_checksum: "sha256:ddd".into(),
                dest_path: DestPath::from("hooks/pre-push/hook.sh"),
            },
        );

        let (promoted, diagnostics) = promote_v1_items(v1_items);

        // Both entries must be present — neither was silently dropped.
        assert_eq!(promoted.len(), 2, "both items should survive promotion");
        assert_eq!(diagnostics.len(), 1);

        // The first item gets the canonical key; the second gets the fallback dest_path key.
        let checksums: std::collections::HashSet<String> = promoted
            .values()
            .map(|v| v.source_checksum.as_ref().to_string())
            .collect();
        assert!(
            checksums.contains("sha256:aaa"),
            "pre-commit hook must be present"
        );
        assert!(
            checksums.contains("sha256:ccc"),
            "pre-push hook must be present"
        );
    }

    #[test]
    fn load_with_diagnostics_reports_v1_promotion_collision() {
        let v1_toml = r#"
version = 1

[dependencies.base]
url = "https://github.com/org/base.git"

[items."hooks/pre-commit/hook.sh"]
source = "base"
kind = "hook"
source_checksum = "sha256:aaa"
installed_checksum = "sha256:bbb"
dest_path = "hooks/pre-commit/hook.sh"

[items."hooks/pre-push/hook.sh"]
source = "base"
kind = "hook"
source_checksum = "sha256:ccc"
installed_checksum = "sha256:ddd"
dest_path = "hooks/pre-push/hook.sh"
"#;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.lock"), v1_toml).unwrap();

        let (lock, diagnostics) = load_with_diagnostics(dir.path()).unwrap();

        assert_eq!(lock.version, LOCK_VERSION);
        assert_eq!(lock.items.len(), 2);
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.level,
            crate::diagnostic::DiagnosticLevel::Warning
        );
        assert_eq!(diagnostic.code, "lock-promotion-collision");
        assert!(diagnostic.message.contains("key collision"));
        assert!(diagnostic.message.contains("hook/hooks/pre-push/hook.sh"));
    }

    #[test]
    fn build_rejects_empty_checksums_from_carried_items() {
        let graph = ResolvedGraph {
            nodes: IndexMap::new(),
            order: Vec::new(),
            filters: HashMap::new(),
        };
        let old_lock = LockFile {
            version: LOCK_VERSION,
            dependencies: IndexMap::new(),
            items: IndexMap::from([(
                "agent/coder".to_string(),
                LockedItemV2 {
                    source: "base".into(),
                    kind: ItemKind::Agent,
                    version: None,
                    source_checksum: "".into(),
                    outputs: vec![OutputRecord {
                        target_root: ".mars".to_string(),
                        dest_path: DestPath::from("agents/coder.md"),
                        installed_checksum: "sha256:installed".into(),
                    }],
                },
            )]),
        };
        let applied = ApplyResult {
            outcomes: vec![ActionOutcome {
                item_id: ItemId {
                    kind: ItemKind::Agent,
                    name: "coder".into(),
                },
                action: ActionTaken::Skipped,
                dest_path: "agents/coder.md".into(),
                source_name: "base".into(),
                source_checksum: None,
                installed_checksum: None,
            }],
        };

        let err = build(&graph, &applied, &old_lock).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty source_checksum"));
    }
}
