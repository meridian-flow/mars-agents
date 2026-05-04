//! `mars link <target>` — add a managed target directory.
//!
//! `mars link <target>` adds the target to `settings.targets` and copies
//! content from `.mars/` into that target.
//! Use `mars unlink <target>` to remove a target.

use crate::diagnostic::{Diagnostic, DiagnosticCategory, DiagnosticCollector, DiagnosticLevel};
use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind, LockFile};
use crate::sync::apply::{ActionOutcome, ActionTaken};
use crate::types::ItemName;
use std::collections::HashSet;

use super::output;

/// Arguments for `mars link`.
#[derive(Debug, clap::Args)]
pub struct LinkArgs {
    /// Target directory to materialize (e.g. `.claude`).
    pub target: String,
}

/// Run `mars link`.
pub fn run(args: &LinkArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let target_name = super::target::normalize_target_name(&args.target)?;
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
    let agent_surface_policy = crate::compiler::agent_surface_policy(
        config.settings.agent_emission.as_ref(),
        ctx.meridian_managed,
    );
    let suppressed_outcomes;
    let sync_outcomes = if matches!(
        agent_surface_policy,
        crate::compiler::AgentSurfacePolicy::SuppressAll
    ) {
        suppressed_outcomes = crate::compiler::suppress_agent_outcomes(&outcomes);
        &suppressed_outcomes
    } else {
        &outcomes
    };
    let previous_managed_paths = lock
        .all_output_dest_paths()
        .map(|dest_path| dest_path.to_string())
        .collect::<HashSet<String>>();

    let mut diag = DiagnosticCollector::new();
    let target_outcomes = crate::target_sync::sync_managed_targets(
        &ctx.project_root,
        &mars_dir,
        &[target_name.to_string()],
        sync_outcomes,
        &previous_managed_paths,
        true,
        &mut diag,
    );
    let mut diagnostics = diag.drain();
    if let Some(diagnostic) = deprecated_agents_target_diagnostic(target_name) {
        diagnostics.push(diagnostic);
    }

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

fn deprecated_agents_target_diagnostic(target_name: &str) -> Option<Diagnostic> {
    (target_name == ".agents").then(|| Diagnostic {
        level: DiagnosticLevel::Warning,
        code: "deprecated-agents-target",
        message: "`.agents` is a deprecated link target. Run `mars unlink .agents` to remove it. Skills are now emitted to native harness dirs automatically.".to_string(),
        context: Some("link target".to_string()),
        category: Some(DiagnosticCategory::Compatibility),
    })
}

fn lock_items_as_sync_outcomes(lock: &LockFile) -> Vec<ActionOutcome> {
    lock.flat_items()
        .into_iter()
        .map(|(dest_path, item)| ActionOutcome {
            item_id: ItemId {
                kind: item.kind,
                name: item_name_from_dest_path(&dest_path, item.kind),
            },
            action: ActionTaken::Skipped,
            dest_path,
            source_name: item.source,
            source_checksum: None,
            installed_checksum: Some(item.installed_checksum),
        })
        .collect()
}

fn item_name_from_dest_path(dest_path: &crate::types::DestPath, kind: ItemKind) -> ItemName {
    let last = dest_path.as_str().rsplit('/').next().unwrap_or("");
    let name = match kind {
        ItemKind::Agent => last.strip_suffix(".md").unwrap_or(last).to_string(),
        ItemKind::Skill | ItemKind::Hook | ItemKind::McpServer | ItemKind::BootstrapDoc => {
            last.to_string()
        }
    };

    ItemName::from(name)
}
