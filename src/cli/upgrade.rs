//! `mars upgrade` — upgrade dependencies to newest versions.

use crate::error::MarsError;
use crate::sync::{DependencyUpsertChange, ResolutionMode, SyncOptions, SyncRequest};
use crate::types::SourceName;

use super::output;

/// Arguments for `mars upgrade`.
#[derive(Debug, clap::Args)]
pub struct UpgradeArgs {
    /// Specific dependencies to upgrade (default: all).
    pub names: Vec<String>,

    /// Bump direct dependency version constraints to resolved latest tags.
    #[arg(long)]
    pub bump: bool,
}

/// Run `mars upgrade`.
pub fn run(args: &UpgradeArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let request = SyncRequest {
        resolution: ResolutionMode::Maximize {
            targets: args
                .names
                .iter()
                .map(|s| SourceName::from(s.as_str()))
                .collect(),
            bump: args.bump,
        },
        mutation: None,
        options: SyncOptions::default(),
    };

    let report = crate::sync::execute(ctx, &request)?;

    if args.bump && !json {
        print_bump_messages(&report.dependency_changes);
    }
    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}

fn print_bump_messages(changes: &[DependencyUpsertChange]) {
    for change in changes {
        if change.old_version == change.new_version {
            continue;
        }
        let from = change.old_version.as_deref().unwrap_or("latest");
        let to = change.new_version.as_deref().unwrap_or("latest");
        output::print_info(&format!(
            "bumped dependency `{}`: {from} -> {to}",
            change.name
        ));
    }
}
