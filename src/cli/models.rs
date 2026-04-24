//! CLI handlers for `mars models` subcommands.
#![allow(clippy::print_literal)]

use clap::{Parser, Subcommand};
use indexmap::IndexMap;

use crate::error::MarsError;
use crate::models::{self, HarnessSource, ModelAlias, ModelSpec};
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
    /// List all model aliases (consumer + deps) with resolved IDs.
    List(ListArgs),
    /// Show resolution chain for a specific alias.
    Resolve(ResolveAliasArgs),
    /// Quick-add a pinned alias to mars.toml [models].
    Alias(AddAliasArgs),
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Show all aliases including those without an available harness.
    #[arg(long)]
    all: bool,
    /// Skip automatic models-cache refresh; use whatever's on disk (equivalent to MARS_OFFLINE=1).
    #[arg(long)]
    no_refresh_models: bool,
    /// Only show aliases matching these patterns (overrides config).
    #[arg(long, value_delimiter = ',', conflicts_with = "exclude")]
    include: Option<Vec<String>>,
    /// Hide aliases matching these patterns (overrides config).
    #[arg(long, value_delimiter = ',', conflicts_with = "include")]
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Parser)]
pub struct ResolveAliasArgs {
    /// Alias name to resolve.
    pub name: String,
    /// Skip automatic models-cache refresh; use whatever's on disk (equivalent to MARS_OFFLINE=1).
    #[arg(long)]
    no_refresh_models: bool,
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
        ModelsCommand::List(args) => run_list(args, ctx, json),
        ModelsCommand::Resolve(a) => run_resolve(a, ctx, json),
        ModelsCommand::Alias(a) => run_alias(a, ctx, json),
    }
}

fn mars_dir(ctx: &MarsContext) -> std::path::PathBuf {
    ctx.project_root.join(".mars")
}

fn run_refresh(ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars = mars_dir(ctx);
    let ttl = models::load_models_cache_ttl(ctx);
    eprint!("Fetching models catalog... ");

    let (cache, outcome) = models::ensure_fresh(&mars, ttl, models::RefreshMode::Force)?;
    let count = cache.models.len();
    let cache_warning = cache_warning(&outcome);

    if let Some(warning) = cache_warning.as_deref() {
        eprintln!("warning: {warning}");
    } else if !json {
        eprintln!("done.");
    }

    if json {
        let out = serde_json::json!({
            "status": "ok",
            "models_count": count,
            "fetched_at": cache.fetched_at,
        });
        let mut out = out;
        if let Some(warning) = cache_warning.as_deref() {
            out["cache_warning"] = serde_json::json!(warning);
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        if cache_warning.is_some() {
            println!(
                "Using stale models cache with {} models in .mars/models-cache.json",
                count
            );
        } else {
            println!("Cached {} models in .mars/models-cache.json", count);
        }
    }

    Ok(0)
}

fn run_list(args: &ListArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mars = mars_dir(ctx);
    let ttl = models::load_models_cache_ttl(ctx);
    let mode = models::resolve_refresh_mode(args.no_refresh_models);
    let Some((cache, outcome)) = ensure_fresh_or_json_error(&mars, ttl, mode, json)? else {
        return Ok(1);
    };
    let cache_warning = cache_warning(&outcome);

    // Load config to get consumer models + trigger merge
    let merged = load_merged_aliases(ctx)?;
    let resolved = models::resolve_all(&merged, &cache);

    // Build effective visibility: CLI overrides config entirely.
    let config_visibility = crate::config::load(&ctx.project_root)
        .map(|c| c.settings.model_visibility)
        .unwrap_or_default();

    let visibility = if args.include.is_some() || args.exclude.is_some() {
        crate::config::ModelVisibility {
            include: args.include.clone(),
            exclude: args.exclude.clone(),
        }
    } else {
        config_visibility
    };

    let resolved = models::filter_by_visibility(resolved, &visibility);

    if json {
        let entries: Vec<serde_json::Value> = resolved
            .values()
            .map(|r| {
                let mode = mode_for_alias(merged.get(&r.name).map(|a| &a.spec));
                let mut obj = serde_json::json!({
                    "name": r.name,
                    "harness": r.harness,
                    "harness_source": r.harness_source,
                    "harness_candidates": r.harness_candidates,
                    "provider": r.provider,
                    "mode": mode,
                    "model_id": r.model_id,
                    "resolved_model": r.model_id,
                    "description": r.description,
                });
                if let Some(error) = unavailable_harness_error(r) {
                    obj["error"] = serde_json::json!(error);
                }
                obj
            })
            .collect();
        let mut out = serde_json::json!({
            "aliases": entries,
            "cache_available": cache.fetched_at.is_some(),
        });
        if let Some(warning) = cache_warning.as_deref() {
            out["cache_warning"] = serde_json::json!(warning);
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        if let Some(warning) = cache_warning.as_deref() {
            eprintln!("warning: {warning}");
        }
        // Table output
        println!(
            "{:<12} {:<10} {:<14} {:<30} {}",
            "ALIAS", "HARNESS", "MODE", "RESOLVED", "DESCRIPTION"
        );
        for r in resolved.values() {
            if !args.all && r.harness_source == HarnessSource::Unavailable {
                continue;
            }
            let harness = r.harness.as_deref().unwrap_or("—");
            let mode = mode_for_alias(merged.get(&r.name).map(|a| &a.spec));
            let desc = if r.harness_source == HarnessSource::Unavailable {
                format!("(install: {})", r.harness_candidates.join(", "))
            } else {
                r.description.clone().unwrap_or_default()
            };
            println!(
                "{:<12} {:<10} {:<14} {:<30} {}",
                r.name, harness, mode, r.model_id, desc
            );
        }
    }

    Ok(0)
}

fn run_resolve(args: &ResolveAliasArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let merged = load_merged_aliases(ctx)?;
    let mars = mars_dir(ctx);
    let ttl = models::load_models_cache_ttl(ctx);
    let mode = models::resolve_refresh_mode(args.no_refresh_models);
    let Some((cache, outcome)) = ensure_fresh_or_json_error(&mars, ttl, mode, json)? else {
        return Ok(1);
    };

    // Step 1: exact alias lookup
    if let Some(alias) = merged.get(&args.name) {
        return run_resolve_exact_alias(&args.name, alias, &merged, ctx, &cache, &outcome, json);
    }

    // Step 2: alias-prefix resolution
    if let Some(resolved) = models::resolve_with_alias_prefix(&args.name, &merged, &cache) {
        return run_output_resolved(&args.name, &resolved, "alias_prefix", &outcome, json);
    }

    // Step 3: passthrough
    run_output_passthrough(&args.name, &outcome, json)
}

fn run_alias(args: &AddAliasArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let mut config = crate::config::load(&ctx.project_root)?;
    config.models.insert(
        args.name.clone(),
        ModelAlias {
            harness: Some(args.harness.clone()),
            description: args.description.clone(),
            spec: ModelSpec::Pinned {
                model: args.model_id.clone(),
                provider: None,
            },
        },
    );
    crate::config::save(&ctx.project_root, &config)?;

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

fn ensure_fresh_or_json_error(
    mars: &std::path::Path,
    ttl: u32,
    mode: models::RefreshMode,
    json: bool,
) -> Result<Option<(models::ModelsCache, models::RefreshOutcome)>, MarsError> {
    match models::ensure_fresh(mars, ttl, mode) {
        Ok(ok) => Ok(Some(ok)),
        Err(err @ MarsError::ModelCacheUnavailable { .. }) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "error": format!("{err}"),
                }))
                .unwrap()
            );
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn run_resolve_exact_alias(
    name: &str,
    alias: &ModelAlias,
    merged: &IndexMap<String, ModelAlias>,
    ctx: &MarsContext,
    cache: &models::ModelsCache,
    outcome: &models::RefreshOutcome,
    json: bool,
) -> Result<i32, MarsError> {
    let cache_warning = cache_warning(outcome);
    if let Some(warning) = cache_warning.as_deref()
        && !json
    {
        eprintln!("warning: {warning}");
    }

    let source = determine_source(name, ctx)?;
    let resolved_map = models::resolve_all(merged, cache);
    let resolved_entry = resolved_map.get(name);

    if json {
        if let Some(r) = resolved_entry {
            let mut out = serde_json::json!({
                "name": r.name,
                "source": source,
                "provider": r.provider,
                "harness": r.harness,
                "harness_source": r.harness_source,
                "harness_candidates": r.harness_candidates,
                "model_id": r.model_id,
                "resolved_model": r.model_id,
                "spec": format_spec(&alias.spec),
                "description": r.description,
            });
            if let Some(error) = unavailable_harness_error(r) {
                out["error"] = serde_json::json!(error);
            }
            if let Some(warning) = cache_warning.as_deref() {
                out["cache_warning"] = serde_json::json!(warning);
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        } else {
            let mut out = serde_json::json!({
                "error": format!("alias `{}` did not resolve to a model ID", name),
            });
            if let Some(warning) = cache_warning.as_deref() {
                out["cache_warning"] = serde_json::json!(warning);
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            return Ok(1);
        }
    } else {
        let Some(r) = resolved_entry else {
            eprintln!("error: alias `{}` did not resolve to a model ID", name);
            return Ok(1);
        };
        let harness = r.harness.as_deref().unwrap_or("—");
        println!("Alias:    {}", name);
        println!("Source:   {}", source);
        println!(
            "Harness:  {} ({})",
            harness,
            harness_source_label(&r.harness_source)
        );
        println!("Provider: {}", r.provider);
        match &alias.spec {
            ModelSpec::Pinned { model, provider: _ } => {
                println!("Mode:     pinned");
                println!("Model:    {}", model);
            }
            ModelSpec::AutoResolve {
                provider: _,
                match_patterns,
                exclude_patterns,
            } => {
                println!("Mode:     auto-resolve");
                println!("Match:    {}", match_patterns.join(", "));
                if !exclude_patterns.is_empty() {
                    println!("Exclude:  {}", exclude_patterns.join(", "));
                }
                println!("Resolved: {}", r.model_id);
            }
        }
        if let Some(error) = unavailable_harness_error(r) {
            println!("Error:    {}", error);
        }
        if let Some(desc) = &r.description {
            println!("Desc:     {}", desc);
        }
    }

    Ok(0)
}

fn run_output_resolved(
    name: &str,
    resolved: &models::ResolvedAlias,
    source: &str,
    outcome: &models::RefreshOutcome,
    json: bool,
) -> Result<i32, MarsError> {
    let cache_warning = cache_warning(outcome);
    if let Some(warning) = cache_warning.as_deref()
        && !json
    {
        eprintln!("warning: {warning}");
    }

    if json {
        let mut out = serde_json::json!({
            "name": name,
            "source": source,
            "provider": resolved.provider,
            "harness": resolved.harness,
            "harness_source": resolved.harness_source,
            "harness_candidates": resolved.harness_candidates,
            "model_id": resolved.model_id,
            "resolved_model": resolved.model_id,
            "description": resolved.description,
        });
        if let Some(error) = unavailable_harness_error(resolved) {
            out["error"] = serde_json::json!(error);
        }
        if let Some(warning) = cache_warning.as_deref() {
            out["cache_warning"] = serde_json::json!(warning);
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        let harness = resolved.harness.as_deref().unwrap_or("—");
        println!("Alias:    {}", name);
        println!("Source:   {}", source);
        println!(
            "Harness:  {} ({})",
            harness,
            harness_source_label(&resolved.harness_source)
        );
        println!("Provider: {}", resolved.provider);
        println!("Resolved: {}", resolved.model_id);
        if let Some(error) = unavailable_harness_error(resolved) {
            println!("Error:    {}", error);
        }
        if let Some(desc) = &resolved.description {
            println!("Desc:     {}", desc);
        }
    }

    Ok(0)
}

fn run_output_passthrough(
    name: &str,
    outcome: &models::RefreshOutcome,
    json: bool,
) -> Result<i32, MarsError> {
    if name.trim().is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "error": "model name cannot be empty"
                }))
                .unwrap()
            );
        } else {
            eprintln!("error: model name cannot be empty");
        }
        return Ok(1);
    }

    let cache_warning = cache_warning(outcome);
    if let Some(warning) = cache_warning.as_deref()
        && !json
    {
        eprintln!("warning: {warning}");
    }

    let installed = models::harness::detect_installed_harnesses();
    let guessed_provider = models::infer_provider_from_model_id(name).map(str::to_string);
    let harness = guessed_provider
        .as_deref()
        .and_then(|p| models::harness::resolve_harness_for_provider(p, &installed));
    let harness_source = if harness.is_some() {
        "pattern_guess"
    } else {
        "unavailable"
    };
    let harness_candidates = guessed_provider
        .as_deref()
        .map(models::harness::harness_candidates_for_provider)
        .unwrap_or_default();

    let warning = format!(
        "model '{}' not found in catalog, passing through to harness",
        name
    );

    if json {
        let mut out = serde_json::json!({
            "name": name,
            "source": "passthrough",
            "model_id": name,
            "resolved_model": name,
            "provider": guessed_provider,
            "harness": harness,
            "harness_source": harness_source,
            "harness_candidates": harness_candidates,
            "description": serde_json::Value::Null,
            "warning": warning,
        });
        if let Some(warning) = cache_warning.as_deref() {
            out["cache_warning"] = serde_json::json!(warning);
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        eprintln!("warning: {}", warning);
        let h = harness.as_deref().unwrap_or("—");
        println!("Model:      {}", name);
        println!("Source:     passthrough");
        println!("Harness:    {} ({})", h, harness_source);
        if let Some(provider) = guessed_provider {
            println!("Provider:   {}", provider);
        }
        if !harness_candidates.is_empty() {
            println!("Candidates: {}", harness_candidates.join(", "));
        }
    }

    Ok(0)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Load model aliases by combining cached dependency aliases with consumer config.
fn load_merged_aliases(
    ctx: &MarsContext,
) -> Result<indexmap::IndexMap<String, ModelAlias>, MarsError> {
    // Start with builtins (lowest precedence)
    let mut merged = models::builtin_aliases();

    // Layer dep aliases from cached merge file (overrides builtins)
    let mars_dir = ctx.project_root.join(".mars");
    let merged_path = mars_dir.join("models-merged.json");
    if let Ok(content) = std::fs::read_to_string(&merged_path)
        && let Ok(cached) = serde_json::from_str::<IndexMap<String, ModelAlias>>(&content)
    {
        for (name, alias) in cached {
            merged.insert(name, alias);
        }
    }

    // Layer consumer config on top (highest precedence)
    if let Ok(config) = crate::config::load(&ctx.project_root) {
        for (name, alias) in &config.models {
            merged.insert(name.clone(), alias.clone());
        }
    }

    Ok(merged)
}

/// Determine which layer provides an alias (consumer or dependency).
fn determine_source(name: &str, ctx: &MarsContext) -> Result<String, MarsError> {
    let config = match crate::config::load(&ctx.project_root) {
        Ok(c) => c,
        Err(_) => return Ok("unknown".to_string()),
    };

    if config.models.contains_key(name) {
        return Ok("consumer (mars.toml)".to_string());
    }

    Ok("dependency".to_string())
}

fn format_spec(spec: &ModelSpec) -> serde_json::Value {
    match spec {
        ModelSpec::Pinned { model, provider } => {
            let mut out = serde_json::json!({ "mode": "pinned", "model": model });
            if let Some(provider) = provider {
                out["provider"] = serde_json::json!(provider);
            }
            out
        }
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

fn mode_for_alias(spec: Option<&ModelSpec>) -> &'static str {
    match spec {
        Some(ModelSpec::Pinned { .. }) => "pinned",
        Some(ModelSpec::AutoResolve { .. }) => "auto-resolve",
        None => "unknown",
    }
}

fn harness_source_label(source: &HarnessSource) -> &'static str {
    match source {
        HarnessSource::Explicit => "explicit",
        HarnessSource::AutoDetected => "auto-detected",
        HarnessSource::Unavailable => "unavailable",
    }
}

fn unavailable_harness_error(resolved: &models::ResolvedAlias) -> Option<String> {
    if resolved.harness_source != HarnessSource::Unavailable {
        return None;
    }
    if let Some(h) = &resolved.harness {
        Some(format!("Harness '{}' is not installed", h))
    } else {
        Some(format!(
            "No installed harness for provider '{}'. Install one of: {}",
            resolved.provider,
            resolved.harness_candidates.join(", ")
        ))
    }
}

fn stale_warning(reason: &str) -> String {
    format!("models cache refresh failed: {reason}; using stale cache")
}

fn cache_warning(outcome: &models::RefreshOutcome) -> Option<String> {
    match outcome {
        models::RefreshOutcome::StaleFallback { reason } => Some(stale_warning(reason)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::TempDir;

    fn write_mars_toml(temp: &TempDir, contents: &str) {
        std::fs::write(temp.path().join("mars.toml"), contents).unwrap();
    }

    fn normalized_exit_code(result: Result<i32, MarsError>) -> i32 {
        match result {
            Ok(code) => code,
            Err(err) => err.exit_code(),
        }
    }

    #[test]
    fn list_args_parses_no_refresh_models() {
        let args = ListArgs::try_parse_from(["mars", "--no-refresh-models"]).unwrap();
        assert!(args.no_refresh_models);
    }

    #[test]
    fn resolve_alias_args_parses_no_refresh_models() {
        let args =
            ResolveAliasArgs::try_parse_from(["mars", "opus", "--no-refresh-models"]).unwrap();
        assert!(args.no_refresh_models);
    }

    #[test]
    fn list_no_refresh_without_cache_is_non_zero() {
        let temp = TempDir::new().unwrap();
        write_mars_toml(&temp, "[settings]\n");
        let ctx = MarsContext::new(temp.path().to_path_buf()).unwrap();
        let args = ModelsArgs::try_parse_from(["mars", "list", "--no-refresh-models"]).unwrap();

        let exit = normalized_exit_code(run(&args, &ctx, false));
        assert_ne!(exit, 0);
    }

    #[test]
    fn resolve_no_refresh_without_cache_is_non_zero() {
        let temp = TempDir::new().unwrap();
        write_mars_toml(
            &temp,
            r#"[settings]

[models.opus]
harness = "claude"
model = "claude-opus-4-6"
"#,
        );
        let ctx = MarsContext::new(temp.path().to_path_buf()).unwrap();
        let args =
            ModelsArgs::try_parse_from(["mars", "resolve", "opus", "--no-refresh-models"]).unwrap();

        let exit = normalized_exit_code(run(&args, &ctx, false));
        assert_ne!(exit, 0);
    }

    #[test]
    fn alias_updates_existing_model_entry() {
        let temp = TempDir::new().unwrap();
        write_mars_toml(
            &temp,
            r#"[settings]

[models.fast]
harness = "claude"
model = "claude-3-5-sonnet"
description = "Old alias"
"#,
        );
        let ctx = MarsContext::new(temp.path().to_path_buf()).unwrap();

        let args = AddAliasArgs {
            name: "fast".to_string(),
            model_id: "gpt-5.3-codex".to_string(),
            harness: "codex".to_string(),
            description: Some("Updated alias".to_string()),
        };

        let exit = run_alias(&args, &ctx, false).unwrap();
        assert_eq!(exit, 0);

        let config = crate::config::load(temp.path()).unwrap();
        assert_eq!(config.models.len(), 1);

        let alias = config.models.get("fast").unwrap();
        assert_eq!(alias.harness.as_deref(), Some("codex"));
        assert_eq!(alias.description.as_deref(), Some("Updated alias"));
        match &alias.spec {
            ModelSpec::Pinned { model, provider } => {
                assert_eq!(model, "gpt-5.3-codex");
                assert_eq!(provider, &None);
            }
            _ => panic!("expected pinned alias"),
        }
    }
}
