use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::lock::ItemId;

/// An item discovered in a source tree by filesystem convention.
///
/// Discovery scans for `agents/*.md` and `skills/*/SKILL.md`.
/// The manifest is not consulted for what a package provides.
#[derive(Debug, Clone)]
pub struct DiscoveredItem {
    pub id: ItemId,
    /// Path within source tree (relative).
    pub source_path: PathBuf,
}

/// Discover all installable items in a source tree by filesystem convention.
///
/// Convention:
/// - `agents/*.md` files become `ItemKind::Agent` items
/// - `skills/*/SKILL.md` directories become `ItemKind::Skill` items
/// - Everything else is ignored
///
/// Sources without a `mars.toml` work identically — discovery doesn't
/// depend on the manifest.
pub fn discover_source(tree_path: &Path) -> Result<Vec<DiscoveredItem>, MarsError> {
    let _ = tree_path;
    todo!()
}
