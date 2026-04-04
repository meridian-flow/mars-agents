pub(crate) mod archive;
pub mod git;
pub(crate) mod git_cli;
pub mod parse;
pub mod path;

use std::path::PathBuf;

use crate::error::MarsError;
use crate::types::{CommitHash, SourceName, SourceUrl};

/// Global source cache under `~/.mars/cache` (or `MARS_CACHE_DIR`).
#[derive(Debug, Clone)]
pub struct GlobalCache {
    pub root: PathBuf,
}

impl GlobalCache {
    /// Create a new cache directory, ensuring required subdirs exist.
    ///
    /// Resolution order:
    /// 1. `MARS_CACHE_DIR`
    /// 2. `dirs::home_dir()/.mars/cache`
    /// 3. `{current_working_dir}/.mars/cache` fallback
    pub fn new() -> Result<Self, MarsError> {
        let root = if let Some(cache_dir) = std::env::var_os("MARS_CACHE_DIR") {
            PathBuf::from(cache_dir)
        } else if let Some(home) = dirs::home_dir() {
            home.join(".mars").join("cache")
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".mars")
                .join("cache")
        };

        let cache = Self { root };
        std::fs::create_dir_all(cache.archives_dir())?;
        std::fs::create_dir_all(cache.git_dir())?;
        Ok(cache)
    }

    pub fn archives_dir(&self) -> PathBuf {
        self.root.join("archives")
    }

    pub fn git_dir(&self) -> PathBuf {
        self.root.join("git")
    }
}

/// A resolved source reference — pinned to a specific version/commit.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub source_name: SourceName,
    pub version: Option<semver::Version>,
    /// Original tag name (e.g., "v0.5.2")
    pub version_tag: Option<String>,
    pub commit: Option<CommitHash>,
    pub tree_path: PathBuf,
}

/// Available version from a git remote.
#[derive(Debug, Clone)]
pub struct AvailableVersion {
    pub tag: String,
    pub version: semver::Version,
    pub commit_id: String,
}

/// List available versions from a git remote (for resolution).
pub fn list_versions(
    url: &SourceUrl,
    cache: &GlobalCache,
) -> Result<Vec<AvailableVersion>, MarsError> {
    git::list_versions(url.as_ref(), cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_cache_creates_directory() {
        let cache = GlobalCache::new().unwrap();
        assert!(cache.root.exists());
        if let Some(from_env) = std::env::var_os("MARS_CACHE_DIR") {
            assert_eq!(cache.root, PathBuf::from(from_env));
        } else {
            assert!(cache.root.ends_with(".mars/cache"));
        }
        assert!(cache.archives_dir().exists());
        assert!(cache.git_dir().exists());
    }

    #[test]
    fn global_cache_idempotent() {
        let cache1 = GlobalCache::new().unwrap();
        let cache2 = GlobalCache::new().unwrap();
        assert_eq!(cache1.root, cache2.root);
        assert!(cache1.root.exists());
    }
}
