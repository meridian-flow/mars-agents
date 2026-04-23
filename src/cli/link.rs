//! `mars link <dir>` — manage target directories materialized from `.mars/`.
//!
//! `mars link <target>` adds the target to `settings.targets` and copies
//! content from `.mars/` into that target.
//! `mars link --unlink <target>` removes the target from `settings.targets`
//! and removes the target directory.

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind, LockFile};
use crate::sync::apply::{ActionOutcome, ActionTaken};
use crate::types::ItemName;
use std::collections::HashSet;
use std::path::PathBuf;

use super::output;

/// Arguments for `mars link`.
#[derive(Debug, clap::Args)]
pub struct LinkArgs {
    /// Target directory to materialize (e.g. `.claude`).
    pub target: String,

    /// Remove target management instead of adding it.
    #[arg(long)]
    pub unlink: bool,
}

/// Run `mars link`.
pub fn run(args: &LinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let target_name = normalize_target_name(&args.target)?;

    if args.unlink {
        return unlink_target(ctx, &target_name, json);
    }

    link_target(ctx, &target_name, json)
}

fn link_target(ctx: &super::MarsContext, target_name: &str, json: bool) -> Result<i32, MarsError> {
    let config_path = ctx.project_root.join("mars.toml");
    if !config_path.exists() {
        return Err(MarsError::Link {
            target: target_name.to_string(),
            message: format!(
                "mars.toml not found at {} — run `mars init` first",
                ctx.project_root.display()
            ),
        });
    }

    if !json
        && !super::WELL_KNOWN.contains(&target_name)
        && !super::TOOL_DIRS.contains(&target_name)
    {
        output::print_warn(&format!(
            "`{target_name}` is not a recognized tool directory — managing anyway"
        ));
    }

    let mars_dir = ctx.project_root.join(".mars");
    std::fs::create_dir_all(&mars_dir)?;
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut config = crate::config::load(&ctx.project_root)?;
    let mut targets = config
        .settings
        .targets
        .clone()
        .unwrap_or_else(|| config.settings.managed_targets());
    if !targets.iter().any(|target| target == target_name) {
        targets.push(target_name.to_string());
    }

    let settings_changed = config.settings.targets.as_ref() != Some(&targets);

    let lock = crate::lock::load(&ctx.project_root)?;
    let outcomes = lock_items_as_sync_outcomes(&lock);
    let previous_managed_paths = lock
        .items
        .keys()
        .map(|dest_path| PathBuf::from(dest_path.as_str()))
        .collect::<HashSet<PathBuf>>();

    let mut diag = DiagnosticCollector::new();
    let target_outcomes = crate::target_sync::sync_managed_targets(
        &ctx.project_root,
        &mars_dir,
        &[target_name.to_string()],
        &outcomes,
        &previous_managed_paths,
        true,
        &mut diag,
    );
    let diagnostics = diag.drain();

    let Some(outcome) = target_outcomes.first() else {
        return Err(MarsError::Link {
            target: target_name.to_string(),
            message: "target sync produced no result".to_string(),
        });
    };

    if !outcome.errors.is_empty() {
        return Err(MarsError::Link {
            target: target_name.to_string(),
            message: outcome.errors.join("; "),
        });
    }

    if settings_changed {
        config.settings.targets = Some(targets);
        crate::config::save(&ctx.project_root, &config)?;
    }

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "target": target_name,
            "settings_updated": settings_changed,
            "synced": outcome.items_synced,
            "removed": outcome.items_removed,
            "diagnostics": diagnostics,
        }));
    } else {
        output::print_success(&format!(
            "managed target `{target_name}` (synced {}, removed {})",
            outcome.items_synced, outcome.items_removed
        ));
        for diagnostic in diagnostics {
            output::print_warn(&diagnostic.to_string());
        }
    }

    Ok(0)
}

fn unlink_target(
    ctx: &super::MarsContext,
    target_name: &str,
    json: bool,
) -> Result<i32, MarsError> {
    let mars_dir = ctx.project_root.join(".mars");
    std::fs::create_dir_all(&mars_dir)?;
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut config = crate::config::load(&ctx.project_root)?;
    let mut settings_updated = false;
    let mut target_was_managed = false;

    if let Some(targets) = config.settings.targets.as_mut() {
        let old_len = targets.len();
        targets.retain(|target| target != target_name);
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

    let target_dir = ctx.project_root.join(target_name);
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
    } else {
        output::print_info(&format!("removed `{target_name}` from settings.targets"));
    }

    Ok(0)
}

fn normalize_target_name(target: &str) -> Result<String, MarsError> {
    let normalized = target.trim_end_matches('/').trim_end_matches('\\');
    if normalized.contains('/') || normalized.contains('\\') {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "link target must be a directory name, not a path".to_string(),
        });
    }
    if normalized.is_empty() || normalized == "." || normalized == ".." {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "invalid link target name".to_string(),
        });
    }
    Ok(normalized.to_string())
}

fn lock_items_as_sync_outcomes(lock: &LockFile) -> Vec<ActionOutcome> {
    lock.items
        .values()
        .map(|item| ActionOutcome {
            item_id: ItemId {
                kind: item.kind,
                name: item_name_from_dest_path(&item.dest_path, item.kind),
            },
            action: ActionTaken::Skipped,
            dest_path: item.dest_path.clone(),
            source_name: item.source.clone(),
            source_checksum: None,
            installed_checksum: Some(item.installed_checksum.clone()),
        })
        .collect()
}

fn item_name_from_dest_path(dest_path: &crate::types::DestPath, kind: ItemKind) -> ItemName {
    let last = dest_path.as_str().rsplit('/').next().unwrap_or("");
    let name = match kind {
        ItemKind::Agent => last.strip_suffix(".md").unwrap_or(last).to_string(),
        ItemKind::Skill => last.to_string(),
    };

    ItemName::from(name)
}

#[cfg(test)]
mod tests {
    use super::normalize_target_name;

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize_target_name(".claude/").unwrap(), ".claude");
    }

    #[test]
    fn normalize_rejects_path() {
        assert!(normalize_target_name("foo/bar").is_err());
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_target_name("").is_err());
    }

    #[test]
    fn normalize_rejects_dots() {
        assert!(normalize_target_name(".").is_err());
        assert!(normalize_target_name("..").is_err());
    }
}
