pub mod git;
pub mod path;

use std::path::{Path, PathBuf};

use crate::config::SourceSpec;
use crate::error::MarsError;

/// Cache directory for fetched sources.
///
/// Layout: `{root}/.mars/cache/{url_to_dirname}/`
#[derive(Debug, Clone)]
pub struct CacheDir {
    pub path: PathBuf,
}

impl CacheDir {
    /// Create a new cache directory reference, ensuring it exists.
    pub fn new(root: &Path) -> Result<Self, MarsError> {
        let path = root.join(".mars").join("cache");
        std::fs::create_dir_all(&path)?;
        Ok(CacheDir { path })
    }
}

/// A resolved source reference — pinned to a specific version/commit.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub source_name: String,
    pub version: Option<semver::Version>,
    /// Original tag name (e.g., "v0.5.2")
    pub version_tag: Option<String>,
    pub commit: Option<String>,
    pub tree_path: PathBuf,
}

/// Available version from a git remote.
#[derive(Debug, Clone)]
pub struct AvailableVersion {
    pub tag: String,
    pub version: semver::Version,
    pub commit_id: git2::Oid,
}

/// Trait for source fetching — git and path implement this.
///
/// Separates version resolution (listing tags, matching constraints) from
/// content fetching (cloning, updating cache).
pub trait SourceFetcher {
    /// Resolve version constraints to a concrete ref/version.
    fn resolve(&self, spec: &SourceSpec, cache: &CacheDir) -> Result<ResolvedRef, MarsError>;

    /// Fetch/update source content to cache, return path to source tree.
    fn fetch(&self, resolved: &ResolvedRef, cache: &CacheDir) -> Result<PathBuf, MarsError>;
}

/// Collection of available fetchers (git, path).
#[derive(Debug)]
pub struct Fetchers {
    _private: (),
}

impl Default for Fetchers {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetchers {
    /// Create the default set of fetchers.
    pub fn new() -> Self {
        Fetchers { _private: () }
    }
}

/// Dispatch to the right fetcher based on source spec.
pub fn fetch_source(
    spec: &SourceSpec,
    source_name: &str,
    cache_dir: &Path,
    project_root: &Path,
) -> Result<ResolvedRef, MarsError> {
    match spec {
        SourceSpec::Git(git_spec) => {
            git::fetch(&git_spec.url, git_spec.version.as_deref(), source_name, cache_dir)
        }
        SourceSpec::Path(p) => path::fetch_path(p, project_root, source_name),
    }
}

/// List available versions from a git remote (for resolution).
pub fn list_versions(url: &str, cache_dir: &Path) -> Result<Vec<AvailableVersion>, MarsError> {
    git::list_versions(url, cache_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn cache_dir_creates_directory() {
        let dir = TempDir::new().unwrap();
        let cache = CacheDir::new(dir.path()).unwrap();
        assert!(cache.path.exists());
        assert!(cache.path.ends_with(".mars/cache"));
    }

    #[test]
    fn cache_dir_idempotent() {
        let dir = TempDir::new().unwrap();
        let cache1 = CacheDir::new(dir.path()).unwrap();
        let cache2 = CacheDir::new(dir.path()).unwrap();
        assert_eq!(cache1.path, cache2.path);
        assert!(cache1.path.exists());
    }
}
