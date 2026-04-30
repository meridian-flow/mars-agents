//! `mars adopt <path>` — move unmanaged target content into `.mars-src/`, then sync.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::error::MarsError;
use crate::local_source;
use crate::lock::ItemKind;
use crate::sync::{ResolutionMode, SyncOptions, SyncRequest};
use crate::types::{DestPath, MarsContext};

use super::output;

#[derive(Debug, clap::Args)]
pub struct AdoptArgs {
    /// Path to an unmanaged item under a managed target directory.
    pub path: PathBuf,

    /// Show what would happen without moving content or syncing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug)]
struct AdoptPlan {
    kind: ItemKind,
    name: String,
    source_abs: PathBuf,
    source_display: String,
    dest_abs: PathBuf,
    dest_display: String,
}

#[derive(Debug, Serialize)]
struct AdoptJson<'a> {
    ok: bool,
    kind: &'a str,
    name: &'a str,
    source_path: &'a str,
    dest_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync: Option<serde_json::Value>,
}

pub fn run(args: &AdoptArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let config = crate::config::load(&ctx.project_root)?;

    let lock = crate::lock::load(&ctx.project_root)?;
    let source_abs = resolve_cli_path(&args.path)?;
    let source_display = relative_display(&ctx.project_root, &source_abs);

    if source_abs.symlink_metadata().is_err() {
        return Err(MarsError::InvalidRequest {
            message: format!("path not found: {source_display}"),
        });
    }

    let (target_name, target_rel) = source_target_membership(ctx, &config, &source_abs)?;
    let target_dest = DestPath::new(target_rel.to_string_lossy().as_ref()).map_err(|e| {
        MarsError::InvalidRequest {
            message: format!(
                "{} resolves to invalid managed target item `{}`: {e}",
                source_display,
                target_rel.display()
            ),
        }
    })?;
    if lock.items.contains_key(&target_dest) {
        return Err(MarsError::InvalidRequest {
            message: format!(
                "{source_display} is already managed by Mars (target `{target_name}` item `{}`)",
                target_rel.display()
            ),
        });
    }

    let plan = build_plan(ctx, &source_abs, &source_display)?;

    if args.dry_run {
        return print_dry_run(&plan, json);
    }

    move_item(&plan.source_abs, &plan.dest_abs)?;

    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: None,
        options: SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        },
    };
    let report = crate::sync::execute(ctx, &request)?;

    if json {
        output::print_json(&AdoptJson {
            ok: true,
            kind: kind_name(plan.kind),
            name: &plan.name,
            source_path: &plan.source_display,
            dest_path: &plan.dest_display,
            sync: Some(output::sync_report_json(&report)),
        });
    } else {
        output::print_success(&format!(
            "adopted {} `{}`: {} -> {}",
            kind_name(plan.kind),
            plan.name,
            plan.source_display,
            plan.dest_display
        ));
        output::print_sync_report(&report, false, true);
    }

    Ok(if report.has_conflicts() { 1 } else { 0 })
}

fn resolve_cli_path(path: &Path) -> Result<PathBuf, MarsError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute)
}

fn source_target_membership(
    ctx: &MarsContext,
    config: &Config,
    source_abs: &Path,
) -> Result<(String, PathBuf), MarsError> {
    let source_canon = dunce::canonicalize(source_abs)?;
    for target_name in config.settings.managed_targets() {
        let target_root = ctx.project_root.join(&target_name);
        let Ok(target_canon) = dunce::canonicalize(&target_root) else {
            continue;
        };
        if let Ok(relative) = source_canon.strip_prefix(&target_canon) {
            return Ok((target_name, relative.to_path_buf()));
        }
    }

    Err(MarsError::InvalidRequest {
        message: format!(
            "{} is not inside a managed target directory",
            relative_display(&ctx.project_root, source_abs)
        ),
    })
}

fn build_plan(
    ctx: &MarsContext,
    source_abs: &Path,
    source_display: &str,
) -> Result<AdoptPlan, MarsError> {
    let metadata = source_abs.symlink_metadata()?;
    let preferred_root = local_source::preferred_local_source_root(&ctx.project_root);

    let (kind, name, dest_abs) = if metadata.is_dir() {
        if !source_abs.join("SKILL.md").is_file() {
            return Err(MarsError::InvalidRequest {
                message: format!(
                    "{source_display} is not a valid skill directory (expected a directory containing SKILL.md)"
                ),
            });
        }
        let name = source_abs
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| MarsError::InvalidRequest {
                message: format!("could not derive skill name from {source_display}"),
            })?
            .to_string();
        (
            ItemKind::Skill,
            name.clone(),
            preferred_root.join("skills").join(&name),
        )
    } else if metadata.is_file() {
        let is_agent = source_abs.extension().and_then(|ext| ext.to_str()) == Some("md")
            && source_abs
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                == Some("agents");
        if !is_agent {
            return Err(MarsError::InvalidRequest {
                message: format!(
                    "{source_display} is not a valid agent file (expected a .md file inside agents/)"
                ),
            });
        }
        let name = source_abs
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| MarsError::InvalidRequest {
                message: format!("could not derive agent name from {source_display}"),
            })?
            .to_string();
        (
            ItemKind::Agent,
            name.clone(),
            preferred_root.join("agents").join(format!("{name}.md")),
        )
    } else {
        return Err(MarsError::InvalidRequest {
            message: format!(
                "{source_display} is not a valid item (expected a skill directory or agent markdown file)"
            ),
        });
    };

    if dest_abs.symlink_metadata().is_ok() {
        return Err(MarsError::InvalidRequest {
            message: format!(
                "{} already exists; refusing to overwrite local source content",
                relative_display(&ctx.project_root, &dest_abs)
            ),
        });
    }

    Ok(AdoptPlan {
        kind,
        name,
        source_abs: source_abs.to_path_buf(),
        source_display: source_display.to_string(),
        dest_display: relative_display(&ctx.project_root, &dest_abs),
        dest_abs,
    })
}

fn print_dry_run(plan: &AdoptPlan, json: bool) -> Result<i32, MarsError> {
    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "dry_run": true,
            "kind": kind_name(plan.kind),
            "name": plan.name,
            "source_path": plan.source_display,
            "dest_path": plan.dest_display,
            "sync": serde_json::Value::Null,
        }));
    } else {
        output::print_info(&format!(
            "would adopt {} `{}`: {} -> {}",
            kind_name(plan.kind),
            plan.name,
            plan.source_display,
            plan.dest_display
        ));
    }
    Ok(0)
}

fn move_item(source: &Path, dest: &Path) -> Result<(), MarsError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match std::fs::rename(source, dest) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device_rename(&err) => Err(MarsError::InvalidRequest {
            message: format!(
                "cannot adopt {} across filesystems in MVP; move it onto the same filesystem as the repo first",
                source.display()
            ),
        }),
        Err(err) => Err(err.into()),
    }
}

fn kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Agent => "agent",
        ItemKind::Skill => "skill",
        ItemKind::Hook => "hook",
        ItemKind::McpServer => "mcp-server",
        ItemKind::BootstrapDoc => "bootstrap-doc",
    }
}

fn relative_display(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(unix)]
fn is_cross_device_rename(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(windows)]
fn is_cross_device_rename(err: &std::io::Error) -> bool {
    const ERROR_NOT_SAME_DEVICE: i32 = 17;
    err.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device_rename(_err: &std::io::Error) -> bool {
    false
}
