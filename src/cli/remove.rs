//! `mars remove <source>` — remove a source from config and prune its items.

use crate::error::MarsError;
use crate::sync::{ConfigMutation, ResolutionMode, SyncOptions, SyncRequest};
use crate::types::SourceName;

use super::output;

/// Arguments for `mars remove`.
#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// Name of the source to remove.
    pub source: String,
}

/// Run `mars remove`.
pub fn run(args: &RemoveArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: Some(ConfigMutation::RemoveDependency {
            name: SourceName::from(args.source.as_str()),
        }),
        options: SyncOptions::default(),
    };
    let report = crate::sync::execute(&ctx.project_root, &ctx.managed_root, &request)?;

    if !json {
        output::print_info(&format!("removed dependency `{}`", args.source));
    }

    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}
