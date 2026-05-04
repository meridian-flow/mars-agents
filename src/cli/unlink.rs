//! `mars unlink <target>` — remove a managed target directory.

use crate::error::MarsError;

use super::output;

/// Arguments for `mars unlink`.
#[derive(Debug, clap::Args)]
pub struct UnlinkArgs {
    /// Target directory to remove (e.g. `.agents`).
    pub target: String,
}

/// Run `mars unlink`.
pub fn run(args: &UnlinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let target_name = super::target::normalize_target_name(&args.target)?;

    let mars_dir = ctx.project_root.join(".mars");
    std::fs::create_dir_all(&mars_dir)?;
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut config = crate::config::load(&ctx.project_root)?;
    let mut settings_updated = false;
    let mut target_was_managed = false;

    if config.settings.managed_root.as_deref() == Some(target_name.as_str()) {
        config.settings.managed_root = None;
        settings_updated = true;
        target_was_managed = true;
    }

    if let Some(targets) = config.settings.targets.as_mut() {
        let old_len = targets.len();
        targets.retain(|t| t != &target_name);
        if targets.len() != old_len {
            settings_updated = true;
            target_was_managed = true;
        }
        if targets.is_empty() {
            config.settings.targets = None;
        }
    }

    if settings_updated {
        crate::config::save(&ctx.project_root, &config)?;
    }

    let target_dir = ctx.project_root.join(&target_name);
    let removed_dir = if target_was_managed && target_dir.exists() {
        std::fs::remove_dir_all(&target_dir)?;
        true
    } else {
        false
    };

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "target": target_name,
            "settings_updated": settings_updated,
            "removed_dir": removed_dir,
        }));
    } else if removed_dir {
        output::print_success(&format!("removed managed target `{target_name}`"));
    } else if target_was_managed {
        output::print_info(&format!("removed `{target_name}` from settings"));
    } else {
        output::print_info(&format!("`{target_name}` is not a managed target; no changes made"));
    }

    Ok(0)
}
