//! CLI handlers for `mars models` subcommands.
#![allow(clippy::print_literal)]

use clap::{Parser, Subcommand};

use crate::error::MarsError;
use crate::models::{self, ModelAlias, ModelSpec, ModelsCache};
use crate::types::MarsContext;

/// Manage model aliases and the models cache.
#[derive(Debug, Parser)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: ModelsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Fetch models from API and update the local cache.
    Refresh,
    /// List all model aliases (consumer + deps + builtins) with resolved IDs.
    List,
    /// Show resolution chain for a specific alias.
    Resolve(ResolveAliasArgs),
    /// Quick-add a pinned alias to mars.toml [models].
    Alias(AddAliasArgs),
}

#[derive(Debug, Parser)]
pub struct ResolveAliasArgs {
    /// Alias name to resolve.
    pub name: String,
}

#[derive(Debug, Parser)]
pub struct AddAliasArgs {
    /// Alias name.
    pub name: String,
    /// Model ID to pin.
    pub model_id: String,
    /// Harness for this alias (default: claude).
    #[arg(long, default_value = "claude")]
    pub harness: String,
    /// Optional description.
    #[arg(long)]
    pub description: Option<String>,
}

pub fn run(args: &ModelsArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    match &args.command {
        ModelsCommand::Refresh => run_refresh(ctx, json),
        ModelsCommand::List => run_list(ctx, json),
        ModelsCommand::Resolve(a) => run_resolve(a, ctx, json),
        ModelsCommand::Alias(a) => run_alias(a, ctx, json),
    }
}

fn mars_dir(ctx: &MarsContext) -> std::path::PathBuf {
    ctx.project_root.join(".mars")
}

fn run_refresh(ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars = mars_dir(ctx);
    eprint!("Fetching models catalog... ");

    let fetched = models::fetch_models()?;
    let count = fetched.len();
    let cache = ModelsCache {
        models: fetched,
        fetched_at: Some(now_iso()),
    };
    models::write_cache(&mars, &cache)?;

    if json {
        let out = serde_json::json!({
            "status": "ok",
            "models_count": count,
            "fetched_at": cache.fetched_at,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        eprintln!("done.");
        println!("Cached {} models in .mars/models-cache.json", count);
    }

    Ok(0)
}

fn run_list(ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars = mars_dir(ctx);
    let cache = models::read_cache(&mars)?;

    // Load config to get consumer models + trigger merge
    let merged = load_merged_aliases(ctx)?;
    let resolved = models::resolve_all(&merged, &cache);

    if json {
        let entries: Vec<serde_json::Value> = merged
            .iter()
            .map(|(name, alias)| {
                let resolved_id = resolved.get(name).cloned().unwrap_or_default();
                let mode = match &alias.spec {
                    ModelSpec::Pinned { .. } => "pinned",
                    ModelSpec::AutoResolve { .. } => "auto-resolve",
                };
                serde_json::json!({
                    "name": name,
                    "harness": alias.harness,
                    "mode": mode,
                    "resolved_model": resolved_id,
                    "description": alias.description,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "aliases": entries,
                "cache_available": cache.fetched_at.is_some(),
            }))
            .unwrap()
        );
    } else {
        if cache.fetched_at.is_none() {
            eprintln!("hint: no models cache — run `mars models refresh` for auto-resolve. Using fallback IDs.");
            eprintln!();
        }
        // Table output
        println!(
            "{:<12} {:<10} {:<14} {:<30} {}",
            "ALIAS", "HARNESS", "MODE", "RESOLVED", "DESCRIPTION"
        );
        for (name, alias) in &merged {
            let resolved_id = resolved.get(name).cloned().unwrap_or_else(|| "—".to_string());
            let mode = match &alias.spec {
                ModelSpec::Pinned { .. } => "pinned",
                ModelSpec::AutoResolve { .. } => "auto-resolve",
            };
            let desc = alias.description.as_deref().unwrap_or("");
            println!(
                "{:<12} {:<10} {:<14} {:<30} {}",
                name, alias.harness, mode, resolved_id, desc
            );
        }
    }

    Ok(0)
}

fn run_resolve(args: &ResolveAliasArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars = mars_dir(ctx);
    let cache = models::read_cache(&mars)?;
    let merged = load_merged_aliases(ctx)?;

    let Some(alias) = merged.get(&args.name) else {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "error": format!("unknown alias: {}", args.name),
                }))
                .unwrap()
            );
        } else {
            eprintln!("error: unknown alias `{}`", args.name);
        }
        return Ok(1);
    };

    // Determine source layer
    let source = determine_source(&args.name, ctx)?;
    let resolved_id = models::resolve_all(&merged, &cache)
        .get(&args.name)
        .cloned()
        .unwrap_or_default();

    if json {
        let out = serde_json::json!({
            "name": args.name,
            "source": source,
            "harness": alias.harness,
            "spec": format_spec(&alias.spec),
            "resolved_model": resolved_id,
            "description": alias.description,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("Alias:    {}", args.name);
        println!("Source:   {}", source);
        println!("Harness:  {}", alias.harness);
        match &alias.spec {
            ModelSpec::Pinned { model } => {
                println!("Mode:     pinned");
                println!("Model:    {}", model);
            }
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                println!("Mode:     auto-resolve");
                println!("Provider: {}", provider);
                println!("Match:    {}", match_patterns.join(", "));
                if !exclude_patterns.is_empty() {
                    println!("Exclude:  {}", exclude_patterns.join(", "));
                }
                println!("Resolved: {}", if resolved_id.is_empty() { "—" } else { &resolved_id });
            }
        }
        if let Some(desc) = &alias.description {
            println!("Desc:     {}", desc);
        }
    }

    Ok(0)
}

fn run_alias(args: &AddAliasArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let config_path = ctx.project_root.join("mars.toml");

    // Read existing config
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Build the TOML entry
    let mut entry = format!(
        "\n[models.{}]\nharness = {:?}\nmodel = {:?}\n",
        args.name, args.harness, args.model_id
    );
    if let Some(desc) = &args.description {
        entry.push_str(&format!("description = {:?}\n", desc));
    }

    // Append to mars.toml
    let new_content = if content.is_empty() {
        entry
    } else {
        format!("{}{}", content.trim_end(), entry)
    };
    std::fs::write(&config_path, new_content)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "alias": args.name,
                "model": args.model_id,
                "harness": args.harness,
            }))
            .unwrap()
        );
    } else {
        println!(
            "Added alias `{}` → {} (harness: {})",
            args.name, args.model_id, args.harness
        );
    }

    Ok(0)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load merged model aliases (consumer + deps + builtins).
fn load_merged_aliases(
    ctx: &MarsContext,
) -> Result<indexmap::IndexMap<String, ModelAlias>, MarsError> {
    // Try to load config — if no mars.toml, just return builtins
    let config = match crate::config::load(&ctx.project_root) {
        Ok(c) => c,
        Err(MarsError::Config(crate::error::ConfigError::NotFound { .. })) => {
            return Ok(models::builtin_aliases());
        }
        Err(e) => return Err(e),
    };

    // We don't run full resolution here — just merge consumer + builtins
    // (running full sync for a `models list` would be too heavy).
    // If the user has run sync before, we could read from lock, but for now
    // just use consumer config + builtins.
    let mut diag = crate::diagnostic::DiagnosticCollector::new();
    let merged = models::merge_model_config(&config.models, &[], &mut diag);
    Ok(merged)
}

/// Determine which layer provides an alias (consumer, dep, or builtin).
fn determine_source(name: &str, ctx: &MarsContext) -> Result<String, MarsError> {
    let config = match crate::config::load(&ctx.project_root) {
        Ok(c) => c,
        Err(_) => return Ok("builtin".to_string()),
    };

    if config.models.contains_key(name) {
        return Ok("consumer (mars.toml)".to_string());
    }

    if models::builtin_aliases().contains_key(name) {
        return Ok("builtin".to_string());
    }

    Ok("dependency".to_string())
}

fn format_spec(spec: &ModelSpec) -> serde_json::Value {
    match spec {
        ModelSpec::Pinned { model } => serde_json::json!({ "mode": "pinned", "model": model }),
        ModelSpec::AutoResolve {
            provider,
            match_patterns,
            exclude_patterns,
        } => serde_json::json!({
            "mode": "auto-resolve",
            "provider": provider,
            "match": match_patterns,
            "exclude": exclude_patterns,
        }),
    }
}

fn now_iso() -> String {
    // Simple ISO timestamp without external chrono dep
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Format as a simple timestamp string
    format!("{secs}")
}
