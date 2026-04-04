//! `mars models` — model metadata management.
//!
//! Subcommands:
//! - `refresh` — fetch from models.dev and cache locally
//! - `list` — display cached models as a table
//! - `resolve` — resolve an alias to {harness, model}

use crate::error::MarsError;
use crate::models;

use super::output;

/// Arguments for `mars models`.
#[derive(Debug, clap::Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: ModelsCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ModelsCommand {
    /// Fetch model metadata from models.dev and cache locally.
    Refresh,

    /// List cached models.
    List(ModelsListArgs),

    /// Resolve a model alias from mars.toml [models] config.
    Resolve(ModelsResolveArgs),
}

#[derive(Debug, clap::Args)]
pub struct ModelsListArgs {
    /// Filter by provider (e.g. anthropic, openai, google).
    #[arg(long)]
    pub provider: Option<String>,

    /// Filter by harness (e.g. claude, codex, opencode).
    #[arg(long)]
    pub harness: Option<String>,

    /// Maximum number of models to display.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, clap::Args)]
pub struct ModelsResolveArgs {
    /// Alias to resolve (e.g. "opus", "sonnet", "codex").
    pub alias: String,
}

/// Run `mars models` — root-free (only needs managed_root for cache).
pub fn run(
    args: &ModelsArgs,
    ctx: &super::MarsContext,
    json: bool,
) -> Result<i32, MarsError> {
    match &args.command {
        ModelsCommand::Refresh => run_refresh(ctx, json),
        ModelsCommand::List(list_args) => run_list(list_args, ctx, json),
        ModelsCommand::Resolve(resolve_args) => run_resolve(resolve_args, ctx, json),
    }
}

fn run_refresh(ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    let cache = models::fetch_models()?;
    models::write_cache(&ctx.managed_root, &cache)?;

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "models_count": cache.models.len(),
            "cache_path": ctx.managed_root.join("models-cache.json").display().to_string(),
        }));
    } else {
        output::print_success(&format!(
            "cached {} models (tool_call capable) to {}",
            cache.models.len(),
            ctx.managed_root.join("models-cache.json").display()
        ));
    }

    Ok(0)
}

fn run_list(
    args: &ModelsListArgs,
    ctx: &super::MarsContext,
    json: bool,
) -> Result<i32, MarsError> {
    let cache = models::read_cache(&ctx.managed_root)?;

    let mut filtered: Vec<&models::CachedModel> = cache
        .models
        .iter()
        .filter(|m| {
            if let Some(ref p) = args.provider
                && &m.provider != p
            {
                return false;
            }
            if let Some(ref h) = args.harness
                && &m.harness != h
            {
                return false;
            }
            true
        })
        .collect();

    // Sort by provider, then release date (newest first), then id
    filtered.sort_by(|a, b| {
        (&a.provider, &b.release_date, &a.id).cmp(&(&b.provider, &a.release_date, &b.id))
    });

    if let Some(limit) = args.limit {
        filtered.truncate(limit);
    }

    if json {
        output::print_json(&filtered);
    } else {
        print_models_table(&filtered);
    }

    Ok(0)
}

fn run_resolve(
    args: &ModelsResolveArgs,
    ctx: &super::MarsContext,
    json: bool,
) -> Result<i32, MarsError> {
    let config = crate::config::load(&ctx.project_root)?;
    let aliases = config.models;

    match models::resolve_alias(&aliases, &args.alias) {
        Some(alias) => {
            if json {
                output::print_json(&serde_json::json!({
                    "alias": &args.alias,
                    "harness": &alias.harness,
                    "model": &alias.model,
                    "description": &alias.description,
                }));
            } else {
                if let Some(ref desc) = alias.description {
                    println!("{} → {} {} ({})", args.alias, alias.harness, alias.model, desc);
                } else {
                    println!("{} → {} {}", args.alias, alias.harness, alias.model);
                }
            }
            Ok(0)
        }
        None => {
            if json {
                output::print_json(&serde_json::json!({
                    "error": format!("alias `{}` not found in [models] config", args.alias),
                }));
            } else {
                output::print_error(&format!(
                    "alias `{}` not found in [models] config",
                    args.alias
                ));
            }
            Ok(1)
        }
    }
}

fn print_models_table(models: &[&models::CachedModel]) {
    if models.is_empty() {
        println!("  no models found");
        return;
    }

    // Compute column widths
    let id_w = models.iter().map(|m| m.id.len()).max().unwrap_or(5).max(5);
    let harness_w = models
        .iter()
        .map(|m| m.harness.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let provider_w = models
        .iter()
        .map(|m| m.provider.len())
        .max()
        .unwrap_or(8)
        .max(8);

    // Cap id width for readability
    let id_w = id_w.min(40);

    println!(
        "{:<id_w$}  {:<harness_w$}  {:<provider_w$}  {:<7}  {:<10}  {:>9}  {:>9}",
        "MODEL", "HARNESS", "PROVIDER", "COST", "RELEASED", "CONTEXT", "OUTPUT"
    );

    for m in models {
        let id_display = if m.id.len() > id_w {
            format!("{}…", &m.id[..id_w - 1])
        } else {
            m.id.clone()
        };
        let tier = models::cost_tier(&m.cost);
        let release = m.release_date.as_deref().unwrap_or("-");
        let ctx_k = if m.limits.context > 0 {
            format!("{}k", m.limits.context / 1000)
        } else {
            "-".to_string()
        };
        let out_k = if m.limits.output > 0 {
            format!("{}k", m.limits.output / 1000)
        } else {
            "-".to_string()
        };
        println!(
            "{:<id_w$}  {:<harness_w$}  {:<provider_w$}  {:<7}  {:<10}  {:>9}  {:>9}",
            id_display, m.harness, m.provider, tier, release, ctx_k, out_k
        );
    }

    println!("\n  {} models total", models.len());
}
