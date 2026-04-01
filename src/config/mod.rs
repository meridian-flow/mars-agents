use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::MarsError;

/// User-declared source entry in agents.toml.
///
/// Sources are either git URLs (versioned, fetched via git2) or local paths
/// (unversioned, always syncs current state).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceEntry {
    /// Git source — URL is identity, git tags are versions.
    Git {
        url: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        agents: Option<Vec<String>>,
        #[serde(default)]
        skills: Option<Vec<String>>,
        #[serde(default)]
        exclude: Option<Vec<String>>,
        #[serde(default)]
        rename: Option<IndexMap<String, String>>,
    },
    /// Local path source — for development and project-specific packages.
    Path {
        path: PathBuf,
        #[serde(default)]
        agents: Option<Vec<String>>,
        #[serde(default)]
        skills: Option<Vec<String>>,
        #[serde(default)]
        exclude: Option<Vec<String>>,
        #[serde(default)]
        rename: Option<IndexMap<String, String>>,
    },
}

/// Top-level agents.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub sources: IndexMap<String, SourceEntry>,
    #[serde(default)]
    pub overrides: IndexMap<String, OverrideEntry>,
    #[serde(default)]
    pub settings: Settings,
}

/// Dev override — local path swap for a git source.
///
/// Used in agents.local.toml (gitignored) so each developer can work
/// with local checkouts while production config points at git.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideEntry {
    pub path: PathBuf,
}

/// Global settings — extensible via additional fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {}

/// Resolved source specification after merging config and overrides.
#[derive(Debug, Clone)]
pub enum SourceSpec {
    Git {
        url: String,
        version: Option<String>,
    },
    Path {
        path: PathBuf,
    },
}

/// Git source specification preserved when overrides are active.
#[derive(Debug, Clone)]
pub struct GitSpec {
    pub url: String,
    pub version: Option<String>,
}

/// Effective configuration after merging agents.toml and agents.local.toml.
///
/// This is what the rest of the pipeline operates on.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub sources: IndexMap<String, EffectiveSource>,
    pub settings: Settings,
}

/// A fully-resolved source with override tracking.
#[derive(Debug, Clone)]
pub struct EffectiveSource {
    pub name: String,
    pub spec: SourceSpec,
    pub is_overridden: bool,
    pub original_git: Option<GitSpec>,
    pub agents: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub rename: Option<IndexMap<String, String>>,
}

/// Load and merge agents.toml + agents.local.toml from the given root.
pub fn load(root: &Path) -> Result<EffectiveConfig, MarsError> {
    let _ = root;
    todo!()
}
