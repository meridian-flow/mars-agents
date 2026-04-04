//! Real source provider implementation for the resolver.
//!
//! Bridges the resolver's trait-based interface to the concrete source module.

use std::path::Path;

use crate::config::Manifest;
use crate::error::MarsError;
use crate::resolve::{ManifestReader, SourceFetcher, VersionLister};
use crate::source::{self, AvailableVersion, GlobalCache, ResolvedRef};
use crate::types::CommitHash;

/// Real source provider that delegates to the source module.
///
/// Implements the SourceProvider trait so the resolver can fetch sources
/// and read manifests through a uniform interface.
pub(crate) struct RealSourceProvider<'a> {
    pub cache: &'a GlobalCache,
    pub project_root: &'a Path,
}

impl VersionLister for RealSourceProvider<'_> {
    fn list_versions(
        &self,
        url: &crate::types::SourceUrl,
    ) -> Result<Vec<AvailableVersion>, MarsError> {
        source::list_versions(url, self.cache)
    }
}

impl SourceFetcher for RealSourceProvider<'_> {
    fn fetch_git_version(
        &self,
        url: &crate::types::SourceUrl,
        version: &AvailableVersion,
        source_name: &str,
        preferred_commit: Option<&str>,
    ) -> Result<ResolvedRef, MarsError> {
        let fetch_options = source::git::FetchOptions {
            preferred_commit: preferred_commit.map(CommitHash::from),
        };
        source::git::fetch(
            url.as_ref(),
            Some(&version.tag),
            source_name,
            self.cache,
            &fetch_options,
        )
    }

    fn fetch_git_ref(
        &self,
        url: &crate::types::SourceUrl,
        ref_name: &str,
        source_name: &str,
        preferred_commit: Option<&str>,
    ) -> Result<ResolvedRef, MarsError> {
        let fetch_options = source::git::FetchOptions {
            preferred_commit: preferred_commit.map(CommitHash::from),
        };
        source::git::fetch(
            url.as_ref(),
            Some(ref_name),
            source_name,
            self.cache,
            &fetch_options,
        )
    }

    fn fetch_path(&self, path: &Path, source_name: &str) -> Result<ResolvedRef, MarsError> {
        source::path::fetch_path(path, self.project_root, source_name)
    }
}

impl ManifestReader for RealSourceProvider<'_> {
    fn read_manifest(&self, source_tree: &Path) -> Result<Option<Manifest>, MarsError> {
        crate::config::load_manifest(source_tree)
    }
}
