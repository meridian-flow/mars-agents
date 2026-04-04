//! `mars repair` — rebuild state from lock + dependencies.

use crate::error::{LockError, MarsError};
use crate::lock::LockFile;
use crate::sync::{ResolutionMode, SyncOptions, SyncReport, SyncRequest};

use super::output;

/// Arguments for `mars repair`.
#[derive(Debug, clap::Args)]
pub struct RepairArgs {}

/// Run `mars repair`.
///
/// Re-syncs everything from config. This is effectively a forced sync
/// that rebuilds the state. If lock exists, items are re-installed from
/// dependencies to match it. If lock is missing, a fresh sync is performed.
pub fn run(_args: &RepairArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    if !json {
        output::print_info("repairing — re-syncing from dependencies...");
    }

    let recovered_corrupt_lock = match crate::lock::load(&ctx.project_root) {
        Ok(_) => false,
        Err(MarsError::Lock(LockError::Corrupt { message })) => {
            eprintln!("warning: {message}");
            eprintln!("warning: lock is corrupt, rebuilding from mars.toml + dependencies");
            crate::lock::write(&ctx.project_root, &LockFile::empty())?;
            true
        }
        Err(err) => return Err(err),
    };

    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: None,
        options: SyncOptions {
            force: true,
            dry_run: false,
            frozen: false,
        },
    };

    // Force sync: overwrites everything, rebuilds from dependencies.
    let report = if recovered_corrupt_lock {
        execute_repair_with_collision_cleanup(ctx, &request)?
    } else {
        crate::sync::execute(ctx, &request)?
    };

    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}

fn execute_repair_with_collision_cleanup(
    ctx: &super::MarsContext,
    request: &SyncRequest,
) -> Result<SyncReport, MarsError> {
    const MAX_RETRIES: usize = 1024;
    let mut retries = 0usize;

    loop {
        match crate::sync::execute(ctx, request) {
            Ok(report) => return Ok(report),
            Err(err) => {
                if let Some(path) = extract_unmanaged_collision_path(&err) {
                    if retries >= MAX_RETRIES {
                        return Err(MarsError::InvalidRequest {
                            message: format!(
                                "repair exceeded {MAX_RETRIES} unmanaged-collision retries while rebuilding from corrupt lock"
                            ),
                        });
                    }

                    let mars_dir = ctx.project_root.join(".mars");
                    let full_path = mars_dir.join(path);
                    if full_path.is_dir() {
                        std::fs::remove_dir_all(&full_path)?;
                    } else if full_path.exists() {
                        std::fs::remove_file(&full_path)?;
                    }

                    eprintln!(
                        "warning: removing unmanaged path `{}` to rebuild from corrupt lock",
                        path.display()
                    );
                    retries += 1;
                    continue;
                }

                return Err(err);
            }
        }
    }
}

fn extract_unmanaged_collision_path(err: &MarsError) -> Option<&std::path::Path> {
    match err {
        MarsError::UnmanagedCollision { path, .. } => Some(path.as_path()),
        _ => None,
    }
}
