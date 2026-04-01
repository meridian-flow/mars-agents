//! `mars update` — update sources to newest versions within constraints.

use std::collections::HashSet;
use std::path::Path;

use crate::error::MarsError;

use super::output;

/// Arguments for `mars update`.
#[derive(Debug, clap::Args)]
pub struct UpdateArgs {
    /// Specific sources to update (default: all).
    pub sources: Vec<String>,
}

/// Run `mars update`.
pub fn run(args: &UpdateArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    // Validate that specified sources exist in config
    if !args.sources.is_empty() {
        let config = crate::config::load(root)?;
        for name in &args.sources {
            if !config.sources.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.clone(),
                    message: format!("source `{name}` not found in agents.toml"),
                });
            }
        }
    }

    // Run sync with maximize mode
    // The resolver will pick newest compatible versions
    std::fs::create_dir_all(root.join(".mars").join("cache"))?;

    let cache = crate::source::CacheDir::new(root)?;

    let config = crate::config::load(root)?;
    let local = crate::config::load_local(root)?;
    let effective = crate::config::merge(config, local)?;

    let old_lock = crate::lock::load(root)?;

    // Build resolve options with maximize=true
    let resolve_options = crate::resolve::ResolveOptions {
        maximize: true,
        upgrade_targets: if args.sources.is_empty() {
            HashSet::new() // empty = upgrade all
        } else {
            args.sources.iter().cloned().collect()
        },
    };

    // Use the same provider as the sync pipeline
    let provider = SyncSourceProvider {
        cache_dir: cache.path.clone(),
        project_root: root.to_path_buf(),
    };

    let graph = crate::resolve::resolve(&effective, &provider, Some(&old_lock), &resolve_options)?;

    // Build target state and run through the sync pipeline
    let (mut target_state, renames) =
        crate::sync::target::build_with_collisions(&graph, &effective)?;

    if !renames.is_empty() {
        crate::sync::target::rewrite_skill_refs(&mut target_state, &renames, &graph)?;
    }

    let sync_diff = crate::sync::diff::compute(root, &old_lock, &target_state)?;

    let cache_bases_dir = root.join(".mars").join("cache").join("bases");
    let options = crate::sync::apply::SyncOptions {
        force: false,
        dry_run: false,
        frozen: false,
    };
    let sync_plan = crate::sync::plan::create(&sync_diff, &options, &cache_bases_dir);
    let applied =
        crate::sync::apply::execute(root, &sync_plan, &options, &cache_bases_dir)?;

    let new_lock = crate::lock::build(&graph, &applied, &old_lock)?;
    crate::lock::write(root, &new_lock)?;

    let report = crate::sync::SyncReport {
        applied,
        pruned: Vec::new(),
        warnings: Vec::new(),
    };

    output::print_sync_report(&report, json);

    if report.has_conflicts() {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Source provider for update (same as sync pipeline).
struct SyncSourceProvider {
    cache_dir: std::path::PathBuf,
    project_root: std::path::PathBuf,
}

impl crate::resolve::SourceProvider for SyncSourceProvider {
    fn list_versions(
        &self,
        url: &str,
    ) -> Result<Vec<crate::source::AvailableVersion>, MarsError> {
        crate::source::list_versions(url, &self.cache_dir)
    }

    fn fetch_git_version(
        &self,
        url: &str,
        version: &crate::source::AvailableVersion,
        source_name: &str,
    ) -> Result<crate::source::ResolvedRef, MarsError> {
        crate::source::git::fetch(url, Some(&version.tag), source_name, &self.cache_dir)
    }

    fn fetch_git_ref(
        &self,
        url: &str,
        ref_name: &str,
        source_name: &str,
    ) -> Result<crate::source::ResolvedRef, MarsError> {
        crate::source::git::fetch(url, Some(ref_name), source_name, &self.cache_dir)
    }

    fn fetch_path(
        &self,
        path: &Path,
        source_name: &str,
    ) -> Result<crate::source::ResolvedRef, MarsError> {
        crate::source::path::fetch_path(path, &self.project_root, source_name)
    }

    fn read_manifest(
        &self,
        source_tree: &Path,
    ) -> Result<Option<crate::manifest::Manifest>, MarsError> {
        crate::manifest::load(source_tree)
    }
}
