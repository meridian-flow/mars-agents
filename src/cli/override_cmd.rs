//! `mars override` — set a local dev override for a source.

use crate::error::MarsError;
use crate::sync::{ConfigMutation, ResolutionMode, SyncOptions, SyncRequest};
use crate::types::SourceName;

use super::output;

/// Arguments for `mars override`.
#[derive(Debug, clap::Args)]
pub struct OverrideArgs {
    /// Source name to override.
    pub source: String,

    /// Local path to use instead.
    #[arg(long)]
    pub path: std::path::PathBuf,
}

/// Run `mars override`.
pub fn run(args: &OverrideArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: Some(ConfigMutation::SetOverride {
            source_name: SourceName::from(args.source.as_str()),
            local_path: args.path.clone(),
        }),
        options: SyncOptions::default(),
    };
    let report = crate::sync::execute(&ctx.project_root, &ctx.managed_root, &request)?;

    if !json {
        output::print_success(&format!(
            "override `{}` → {}",
            args.source,
            args.path.display()
        ));
    }
    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}
