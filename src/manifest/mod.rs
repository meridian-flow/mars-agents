use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::MarsError;
use crate::types::{ItemName, SourceUrl};

/// Per-package manifest (mars.toml in package repo root).
///
/// Optional — mars works without it by discovering items from filesystem
/// convention (`agents/*.md`, `skills/*/SKILL.md`). When present, adds
/// declared dependencies on other packages and package metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub dependencies: IndexMap<String, DepSpec>,
}

/// Package metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Dependency specification within a manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DepSpec {
    pub url: SourceUrl,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ItemName>>,
}

const MANIFEST_FILE: &str = "mars.toml";

/// Load mars.toml from a source tree root. Returns None if absent.
pub fn load(source_root: &Path) -> Result<Option<Manifest>, MarsError> {
    let path = source_root.join(MANIFEST_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let manifest: Manifest =
                toml::from_str(&content).map_err(|e| crate::error::ConfigError::Invalid {
                    message: format!("failed to parse {}: {e}", path.display()),
                })?;
            Ok(Some(manifest))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MarsError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_valid_manifest_with_deps() {
        let toml_str = r#"
[package]
name = "my-agents"
version = "1.0.0"
description = "My custom agents"

[dependencies.base]
url = "https://github.com/org/base.git"
version = ">=1.0"
items = ["coder", "reviewer"]

[dependencies.utils]
url = "https://github.com/org/utils.git"
version = ">=0.5"
"#;
        let manifest: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.package.name, "my-agents");
        assert_eq!(manifest.package.version, "1.0.0");
        assert_eq!(
            manifest.package.description.as_deref(),
            Some("My custom agents")
        );
        assert_eq!(manifest.dependencies.len(), 2);

        let base_dep = &manifest.dependencies["base"];
        assert_eq!(base_dep.url, "https://github.com/org/base.git");
        assert_eq!(base_dep.version, ">=1.0");
        let expected_items: Vec<String> = vec!["coder".into(), "reviewer".into()];
        assert_eq!(base_dep.items.as_ref().unwrap(), &expected_items);

        let utils_dep = &manifest.dependencies["utils"];
        assert!(utils_dep.items.is_none());
    }

    #[test]
    fn parse_manifest_without_deps() {
        let toml_str = r#"
[package]
name = "standalone"
version = "0.1.0"
"#;
        let manifest: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.package.name, "standalone");
        assert!(manifest.dependencies.is_empty());
        assert!(manifest.package.description.is_none());
    }

    #[test]
    fn load_returns_none_when_absent() {
        let dir = TempDir::new().unwrap();
        let result = load(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_from_disk() {
        let dir = TempDir::new().unwrap();
        let toml_str = r#"
[package]
name = "test-pkg"
version = "0.2.0"
"#;
        std::fs::write(dir.path().join("mars.toml"), toml_str).unwrap();
        let result = load(dir.path()).unwrap();
        assert!(result.is_some());
        let manifest = result.unwrap();
        assert_eq!(manifest.package.name, "test-pkg");
        assert_eq!(manifest.package.version, "0.2.0");
    }

    #[test]
    fn roundtrip_manifest() {
        let manifest = Manifest {
            package: PackageInfo {
                name: "test".into(),
                version: "1.0.0".into(),
                description: Some("A test package".into()),
            },
            dependencies: {
                let mut m = IndexMap::new();
                m.insert(
                    "dep1".into(),
                    DepSpec {
                        url: "https://github.com/org/dep1.git".into(),
                        version: ">=1.0".into(),
                        items: Some(vec!["agent1".into()]),
                    },
                );
                m
            },
        };
        let serialized = toml::to_string_pretty(&manifest).unwrap();
        let deserialized: Manifest = toml::from_str(&serialized).unwrap();
        assert_eq!(manifest, deserialized);
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "not valid toml {{{}}}").unwrap();
        let result = load(dir.path());
        assert!(result.is_err());
    }
}
