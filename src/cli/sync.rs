//! `mars sync` — resolve + install (make reality match config).

use crate::error::MarsError;
use crate::sync::{ResolutionMode, SyncOptions, SyncRequest};

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
pub fn run(args: &SyncArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: None,
        options: SyncOptions {
            force: args.force,
            dry_run: args.diff,
            frozen: args.frozen,
        },
    };

    let report = crate::sync::execute(ctx, &request)?;

    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}
