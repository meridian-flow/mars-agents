use std::path::PathBuf;

use crate::config::SourceSpec;
use crate::error::MarsError;

/// Cache directory for fetched sources.
///
/// Layout: `.agents/.mars/cache/<url-hash>/`
#[derive(Debug, Clone)]
pub struct CacheDir {
    pub path: PathBuf,
}

impl CacheDir {
    /// Create a new cache directory reference.
    pub fn new(root: &std::path::Path) -> Result<Self, MarsError> {
        let _ = root;
        todo!()
    }
}

/// A resolved source reference — pinned to a specific version/commit.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub source_name: String,
    pub version: Option<semver::Version>,
    pub commit: Option<String>,
    pub tree_path: PathBuf,
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
