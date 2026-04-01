use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Per-package manifest (mars.toml in package repo root).
///
/// Optional — mars works without it by discovering items from filesystem
/// convention (`agents/*.md`, `skills/*/SKILL.md`). When present, adds
/// declared dependencies on other packages and package metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub dependencies: IndexMap<String, DepSpec>,
}

/// Package metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

/// Dependency specification within a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepSpec {
    pub url: String,
    pub version: String,
    #[serde(default)]
    pub items: Option<Vec<String>>,
}
