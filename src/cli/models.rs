//! CLI handlers for `mars models` subcommands.
#![allow(clippy::print_literal)]

use clap::{Parser, Subcommand};
use indexmap::IndexMap;
use std::collections::HashSet;

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
    /// Show all alias-filter candidates (not just winners).
    #[arg(
        long,
        conflicts_with = "catalog",
        conflicts_with = "include",
        conflicts_with = "exclude"
    )]
    all: bool,
    /// Skip automatic models-cache refresh; use whatever's on disk (equivalent to MARS_OFFLINE=1).
    #[arg(long)]
    no_refresh_models: bool,
    /// Only show aliases matching these patterns (overrides config).
    #[arg(
        long,
        value_delimiter = ',',
        conflicts_with = "exclude",
        conflicts_with = "catalog",
        conflicts_with = "all"
    )]
    include: Option<Vec<String>>,
    /// Hide aliases matching these patterns (overrides config).
    #[arg(
        long,
        value_delimiter = ',',
        conflicts_with = "include",
        conflicts_with = "catalog",
        conflicts_with = "all"
    )]
    exclude: Option<Vec<String>>,
    /// Show all models from the cache (not just alias-covered models).
    #[arg(
        long,
        conflicts_with = "include",
        conflicts_with = "exclude",
        conflicts_with = "all"
    )]
    catalog: bool,
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

    if args.catalog {
        return run_list_catalog(&cache, &outcome, json);
    }

    // Load config to get consumer models + trigger merge
    let merged = load_merged_aliases(ctx)?;
    if args.all {
        return run_list_all(&merged, &cache, &outcome, json);
    }

    let cache_warning = cache_warning(&outcome);

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
            if r.harness_source == HarnessSource::Unavailable {
                continue;
            }
            let harness = r.harness.as_deref().unwrap_or("—");
            let mode = mode_for_alias(merged.get(&r.name).map(|a| &a.spec));
            let desc = r.description.clone().unwrap_or_default();
            println!(
                "{:<12} {:<10} {:<14} {:<30} {}",
                r.name, harness, mode, r.model_id, desc
            );
        }
    }

    Ok(0)
}

#[derive(Debug, Clone)]
struct ListModelEntry {
    id: String,
    provider: String,
    release_date: Option<String>,
    harness: Option<String>,
    harness_source: HarnessSource,
    harness_candidates: Vec<String>,
    description: Option<String>,
    matched_aliases: Vec<String>,
}

fn run_list_all(
    merged: &IndexMap<String, ModelAlias>,
    cache: &models::ModelsCache,
    outcome: &models::RefreshOutcome,
    json: bool,
) -> Result<i32, MarsError> {
    let cache_warning = cache_warning(outcome);
    let models = collect_all_model_entries(merged, cache);

    if json {
        let entries: Vec<serde_json::Value> = models
            .into_iter()
            .map(|model| {
                serde_json::json!({
                    "id": model.id,
                    "provider": model.provider,
                    "release_date": model.release_date,
                    "harness": model.harness,
                    "harness_source": model.harness_source,
                    "harness_candidates": model.harness_candidates,
                    "description": model.description,
                    "matched_aliases": model.matched_aliases,
                })
            })
            .collect();
        let mut out = serde_json::json!({
            "models": entries,
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
        println!(
            "{:<10} {:<34} {:<12} {:<10} {}",
            "PROVIDER", "MODEL ID", "RELEASE", "HARNESS", "ALIASES"
        );
        for model in models {
            let release = model.release_date.as_deref().unwrap_or("—");
            let harness = model.harness.as_deref().unwrap_or("—");
            println!(
                "{:<10} {:<34} {:<12} {:<10} {}",
                model.provider,
                model.id,
                release,
                harness,
                model.matched_aliases.join(",")
            );
        }
    }

    Ok(0)
}

fn run_list_catalog(
    cache: &models::ModelsCache,
    outcome: &models::RefreshOutcome,
    json: bool,
) -> Result<i32, MarsError> {
    let cache_warning = cache_warning(outcome);
    let models = collect_catalog_model_entries(cache);

    if json {
        let entries: Vec<serde_json::Value> = models
            .into_iter()
            .map(|model| {
                serde_json::json!({
                    "id": model.id,
                    "provider": model.provider,
                    "release_date": model.release_date,
                    "harness": model.harness,
                    "harness_source": model.harness_source,
                    "harness_candidates": model.harness_candidates,
                    "description": model.description,
                })
            })
            .collect();
        let mut out = serde_json::json!({
            "models": entries,
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
        println!(
            "{:<10} {:<34} {:<12} {:<10}",
            "PROVIDER", "MODEL ID", "RELEASE", "HARNESS"
        );
        for model in models {
            let release = model.release_date.as_deref().unwrap_or("—");
            let harness = model.harness.as_deref().unwrap_or("—");
            println!(
                "{:<10} {:<34} {:<12} {:<10}",
                model.provider, model.id, release, harness
            );
        }
    }

    Ok(0)
}

fn collect_all_model_entries(
    merged: &IndexMap<String, ModelAlias>,
    cache: &models::ModelsCache,
) -> Vec<ListModelEntry> {
    let installed = models::harness::detect_installed_harnesses();
    let mut by_model_id: IndexMap<String, ListModelEntry> = IndexMap::new();

    for (alias_name, alias) in merged {
        match &alias.spec {
            ModelSpec::AutoResolve {
                provider,
                match_patterns,
                exclude_patterns,
            } => {
                for matched in
                    models::auto_resolve_all(provider, match_patterns, exclude_patterns, cache)
                {
                    append_alias_match(&mut by_model_id, matched, &installed, alias_name);
                }
            }
            ModelSpec::Pinned { model, provider } => {
                if let Some(matched) = cache
                    .models
                    .iter()
                    .find(|cache_model| cache_model.id == *model)
                {
                    append_alias_match(&mut by_model_id, matched, &installed, alias_name);
                } else {
                    append_pinned_alias_match(
                        &mut by_model_id,
                        model,
                        provider.as_deref(),
                        alias.description.as_deref(),
                        &installed,
                        alias_name,
                    );
                }
            }
        }
    }

    let mut out: Vec<ListModelEntry> = by_model_id.into_values().collect();
    sort_list_model_entries(&mut out);
    out
}

fn collect_catalog_model_entries(cache: &models::ModelsCache) -> Vec<ListModelEntry> {
    let installed = models::harness::detect_installed_harnesses();
    let mut out: Vec<ListModelEntry> = cache
        .models
        .iter()
        .map(|model| model_entry_for_cached(model, &installed))
        .collect();
    sort_list_model_entries(&mut out);
    out
}

fn append_alias_match(
    by_model_id: &mut IndexMap<String, ListModelEntry>,
    model: &models::CachedModel,
    installed: &HashSet<String>,
    alias_name: &str,
) {
    let entry = by_model_id
        .entry(model.id.clone())
        .or_insert_with(|| model_entry_for_cached(model, installed));

    append_alias_name(entry, alias_name);
}

fn append_pinned_alias_match(
    by_model_id: &mut IndexMap<String, ListModelEntry>,
    model_id: &str,
    provider: Option<&str>,
    description: Option<&str>,
    installed: &HashSet<String>,
    alias_name: &str,
) {
    let entry = by_model_id
        .entry(model_id.to_string())
        .or_insert_with(|| model_entry_for_pinned(model_id, provider, description, installed));

    append_alias_name(entry, alias_name);
}

fn append_alias_name(entry: &mut ListModelEntry, alias_name: &str) {
    if !entry
        .matched_aliases
        .iter()
        .any(|existing| existing == alias_name)
    {
        entry.matched_aliases.push(alias_name.to_string());
    }
}

fn model_entry_for_cached(
    model: &models::CachedModel,
    installed: &HashSet<String>,
) -> ListModelEntry {
    let harness = models::harness::resolve_harness_for_provider(&model.provider, installed);
    let harness_source = if harness.is_some() {
        HarnessSource::AutoDetected
    } else {
        HarnessSource::Unavailable
    };

    ListModelEntry {
        id: model.id.clone(),
        provider: model.provider.clone(),
        release_date: model.release_date.clone(),
        harness,
        harness_source,
        harness_candidates: models::harness::harness_candidates_for_provider(&model.provider),
        description: model.description.clone(),
        matched_aliases: Vec::new(),
    }
}

fn model_entry_for_pinned(
    model_id: &str,
    provider: Option<&str>,
    description: Option<&str>,
    installed: &HashSet<String>,
) -> ListModelEntry {
    let provider = provider
        .map(str::to_string)
        .or_else(|| models::infer_provider_from_model_id(model_id).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let harness = models::harness::resolve_harness_for_provider(&provider, installed);
    let harness_source = if harness.is_some() {
        HarnessSource::AutoDetected
    } else {
        HarnessSource::Unavailable
    };

    ListModelEntry {
        id: model_id.to_string(),
        provider: provider.clone(),
        release_date: None,
        harness,
        harness_source,
        harness_candidates: models::harness::harness_candidates_for_provider(&provider),
        description: description.map(str::to_string),
        matched_aliases: Vec::new(),
    }
}

fn sort_list_model_entries(entries: &mut [ListModelEntry]) {
    entries.sort_by(|a, b| {
        a.provider
            .to_ascii_lowercase()
            .cmp(&b.provider.to_ascii_lowercase())
            .then_with(|| {
                b.release_date
                    .as_deref()
                    .unwrap_or("")
                    .cmp(a.release_date.as_deref().unwrap_or(""))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn run_resolve(args: &ResolveAliasArgs, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    let merged = load_merged_aliases(ctx)?;
    let mars = mars_dir(ctx);
    let ttl = models::load_models_cache_ttl(ctx);
    let mode = models::resolve_refresh_mode(args.no_refresh_models);

    // Cache is enrichment, not a gate. If unavailable, skip to passthrough.
    let cache_result = ensure_fresh_or_json_error(&mars, ttl, mode, json)?;

    if let Some((cache, outcome)) = &cache_result {
        // Step 1: exact alias lookup
        if let Some(alias) = merged.get(&args.name) {
            return run_resolve_exact_alias(&args.name, alias, &merged, ctx, cache, outcome, json);
        }

        // Step 2: alias-prefix resolution
        if let Some(resolved) = models::resolve_with_alias_prefix(&args.name, &merged, cache) {
            return run_output_resolved(&args.name, &resolved, "alias_prefix", outcome, json);
        }
    }

    // Step 3: passthrough — no cache needed
    let outcome = cache_result
        .as_ref()
        .map(|(_, o)| o.clone())
        .unwrap_or(models::RefreshOutcome::Offline);
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
    use indexmap::IndexMap;
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
    fn list_args_parses_catalog() {
        let args = ListArgs::try_parse_from(["mars", "--catalog"]).unwrap();
        assert!(args.catalog);
    }

    #[test]
    fn list_all_and_catalog_conflict() {
        let parsed = ModelsArgs::try_parse_from(["mars", "list", "--all", "--catalog"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn list_all_and_include_conflict() {
        let parsed = ModelsArgs::try_parse_from(["mars", "list", "--all", "--include", "opus"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn list_catalog_and_include_conflict() {
        let parsed = ModelsArgs::try_parse_from(["mars", "list", "--catalog", "--include", "opus"]);
        assert!(parsed.is_err());
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

    fn auto_alias(
        provider: &str,
        match_patterns: &[&str],
        exclude_patterns: &[&str],
    ) -> ModelAlias {
        ModelAlias {
            harness: None,
            description: None,
            spec: ModelSpec::AutoResolve {
                provider: provider.to_string(),
                match_patterns: match_patterns.iter().map(|v| (*v).to_string()).collect(),
                exclude_patterns: exclude_patterns.iter().map(|v| (*v).to_string()).collect(),
            },
        }
    }

    fn pinned_alias(model: &str) -> ModelAlias {
        ModelAlias {
            harness: None,
            description: None,
            spec: ModelSpec::Pinned {
                model: model.to_string(),
                provider: None,
            },
        }
    }

    fn pinned_alias_with_provider(model: &str, provider: &str) -> ModelAlias {
        ModelAlias {
            harness: None,
            description: None,
            spec: ModelSpec::Pinned {
                model: model.to_string(),
                provider: Some(provider.to_string()),
            },
        }
    }

    fn cached_model(id: &str, provider: &str, release_date: Option<&str>) -> models::CachedModel {
        models::CachedModel {
            id: id.to_string(),
            provider: provider.to_string(),
            release_date: release_date.map(|value| value.to_string()),
            description: Some(format!("desc-{id}")),
            context_window: None,
            max_output: None,
        }
    }

    fn cache(models: Vec<models::CachedModel>) -> models::ModelsCache {
        models::ModelsCache {
            models,
            fetched_at: Some("123".to_string()),
        }
    }

    #[test]
    fn list_all_shows_multiple_per_alias() {
        let mut merged = IndexMap::new();
        merged.insert(
            "opus".to_string(),
            auto_alias("Anthropic", &["claude-opus-*"], &[]),
        );

        let models_cache = cache(vec![
            cached_model("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
            cached_model("claude-opus-4-7", "Anthropic", Some("2026-04-01")),
        ]);

        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "claude-opus-4-7");
        assert_eq!(rows[1].id, "claude-opus-4-6");
    }

    #[test]
    fn list_all_includes_matched_aliases_with_dedup() {
        let mut merged = IndexMap::new();
        merged.insert(
            "opus".to_string(),
            auto_alias("Anthropic", &["claude-opus-*"], &[]),
        );
        merged.insert(
            "legacy".to_string(),
            auto_alias("Anthropic", &["*4-6"], &[]),
        );

        let models_cache = cache(vec![cached_model(
            "claude-opus-4-6",
            "Anthropic",
            Some("2026-02-05"),
        )]);

        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "claude-opus-4-6");
        assert_eq!(rows[0].matched_aliases, vec!["opus", "legacy"]);
    }

    #[test]
    fn list_all_includes_pinned_cache_entries() {
        let mut merged = IndexMap::new();
        merged.insert("fixed".to_string(), pinned_alias("gpt-5.3-codex"));

        let models_cache = cache(vec![cached_model(
            "gpt-5.3-codex",
            "OpenAI",
            Some("2026-01-01"),
        )]);
        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "gpt-5.3-codex");
        assert_eq!(rows[0].matched_aliases, vec!["fixed"]);
    }

    #[test]
    fn list_all_includes_pinned_cache_miss_entries() {
        let mut merged = IndexMap::new();
        merged.insert("fixed".to_string(), pinned_alias("gpt-5.3-codex"));

        let models_cache = cache(Vec::new());
        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "gpt-5.3-codex");
        assert!(rows[0].provider.eq_ignore_ascii_case("openai"));
        assert_eq!(rows[0].release_date, None);
        assert_eq!(rows[0].matched_aliases, vec!["fixed"]);
    }

    #[test]
    fn list_all_uses_declared_provider_for_pinned_cache_miss_entries() {
        let mut merged = IndexMap::new();
        merged.insert(
            "custom".to_string(),
            pinned_alias_with_provider("custom-model-id", "Anthropic"),
        );

        let models_cache = cache(Vec::new());
        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "custom-model-id");
        assert_eq!(rows[0].provider, "Anthropic");
        assert_eq!(rows[0].release_date, None);
        assert_eq!(rows[0].matched_aliases, vec!["custom"]);
    }

    #[test]
    fn list_all_includes_unavailable_harness_entries() {
        let mut merged = IndexMap::new();
        merged.insert("x".to_string(), auto_alias("Unknown", &["x-*"], &[]));
        let models_cache = cache(vec![cached_model("x-1", "Unknown", Some("2026-01-01"))]);

        let rows = collect_all_model_entries(&merged, &models_cache);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, None);
        assert_eq!(rows[0].harness_source, HarnessSource::Unavailable);
        assert!(rows[0].harness_candidates.is_empty());
    }

    #[test]
    fn list_catalog_shows_all_cache_sorted() {
        let models_cache = cache(vec![
            cached_model("gpt-5", "OpenAI", Some("2025-06-01")),
            cached_model("claude-opus-4-6", "Anthropic", Some("2026-02-05")),
            cached_model("claude-sonnet-4-5", "Anthropic", Some("2025-08-01")),
        ]);

        let rows = collect_catalog_model_entries(&models_cache);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "claude-opus-4-6");
        assert_eq!(rows[1].id, "claude-sonnet-4-5");
        assert_eq!(rows[2].id, "gpt-5");
    }
}
