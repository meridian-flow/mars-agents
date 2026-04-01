//! `mars sync` — resolve + install (make reality match config).

use std::path::Path;

use crate::error::MarsError;
use crate::source::{CacheDir, Fetchers};
use crate::sync::{SyncContext, SyncReport};
use crate::sync::apply::SyncOptions;

use super::output;

/// Arguments for `mars sync`.
#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Overwrite local modifications for managed files.
    #[arg(long)]
    pub force: bool,

    /// Dry run — show what would change.
    #[arg(long)]
    pub diff: bool,

    /// Install exactly from lock file, error if stale.
    #[arg(long)]
    pub frozen: bool,
}

/// Run `mars sync`.
pub fn run(args: &SyncArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let report = run_sync(root, args.force, args.diff, args.frozen)?;

    output::print_sync_report(&report, json);

    if report.has_conflicts() {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Inner sync function shared by `mars sync`, `mars add`, `mars remove`, etc.
pub fn run_sync(
    root: &Path,
    force: bool,
    dry_run: bool,
    frozen: bool,
) -> Result<SyncReport, MarsError> {
    // Ensure .mars/ dir exists
    std::fs::create_dir_all(root.join(".mars").join("cache"))?;

    let cache = CacheDir::new(root)?;
    let fetchers = Fetchers::new();

    let ctx = SyncContext {
        root: root.to_path_buf(),
        install_target: root.to_path_buf(),
        fetchers,
        cache,
        options: SyncOptions {
            force,
            dry_run,
            frozen,
        },
    };

    crate::sync::sync(&ctx)
}
