//! `mars rename` — rename a managed item.

use crate::error::MarsError;
use crate::sync::{ConfigMutation, ResolutionMode, SyncOptions, SyncRequest};
use crate::types::DestPath;

use super::output;

/// Arguments for `mars rename`.
#[derive(Debug, clap::Args)]
pub struct RenameArgs {
    /// Current item path (e.g., agents/coder__meridian-flow_meridian-base.md).
    pub from: String,
    /// New item path (e.g., agents/coder.md).
    pub to: String,
}

/// Run `mars rename`.
pub fn run(args: &RenameArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(&ctx.project_root)?;

    // Validate `from` is a managed item
    let from_dest = DestPath::new(&args.from).map_err(|e| MarsError::Source {
        source_name: "rename".to_string(),
        message: format!("invalid path `{}`: {e}", args.from),
    })?;
    let _to_dest = DestPath::new(&args.to).map_err(|e| MarsError::Source {
        source_name: "rename".to_string(),
        message: format!("invalid destination path `{}`: {e}", args.to),
    })?;
    if !lock.items.contains_key(&from_dest) {
        return Err(MarsError::Source {
            source_name: "rename".to_string(),
            message: format!("`{}` is not a managed item", args.from),
        });
    }

    let locked_item = &lock.items[&from_dest];
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: Some(ConfigMutation::SetRename {
            source_name: locked_item.source.clone(),
            from: args.from.clone(),
            to: args.to.clone(),
        }),
        options: SyncOptions::default(),
    };

    let report = crate::sync::execute(ctx, &request)?;

    if !json {
        output::print_info(&format!("renamed {} → {}", args.from, args.to));
    }

    output::print_sync_report(&report, json, true);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}
