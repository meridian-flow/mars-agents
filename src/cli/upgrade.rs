//! `mars upgrade` — upgrade sources to newest versions within constraints.

use crate::error::MarsError;
use crate::sync::{ResolutionMode, SyncOptions, SyncRequest};
use crate::types::SourceName;

use super::output;

/// Arguments for `mars upgrade`.
#[derive(Debug, clap::Args)]
pub struct UpgradeArgs {
    /// Specific sources to upgrade (default: all).
    pub sources: Vec<String>,
}

/// Run `mars upgrade`.
pub fn run(args: &UpgradeArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Maximize {
            targets: args
                .sources
                .iter()
                .map(|s| SourceName::from(s.as_str()))
                .collect(),
        },
        mutation: None,
        options: SyncOptions::default(),
    };

    let report = crate::sync::execute(ctx, &request)?;

    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}
