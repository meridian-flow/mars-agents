//! `mars unlink <target>` — remove a managed target directory.

use crate::error::MarsError;

/// Arguments for `mars unlink`.
#[derive(Debug, clap::Args)]
pub struct UnlinkArgs {
    /// Target directory to remove (e.g. `.agents`).
    pub target: String,
}

/// Run `mars unlink`.
pub fn run(args: &UnlinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let target_name = super::link::normalize_target_name(&args.target)?;
    super::link::unlink_target(ctx, &target_name, json)
}
