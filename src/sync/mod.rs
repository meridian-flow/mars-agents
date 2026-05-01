pub mod apply;
pub mod diff;
pub mod filter;
pub mod mutation;
pub mod plan;
pub mod provider;
pub mod rewrite;
pub mod target;
pub mod types;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::path::PathBuf;

use crate::config::{Config, EffectiveConfig, LocalConfig, Settings};
use crate::diagnostic::{Diagnostic, DiagnosticCollector};
use crate::error::MarsError;
use crate::fs::FileLock;
use crate::hash;
use crate::lock::{ItemId, ItemKind};
use crate::lock::{LockFile, LockIndex};
use crate::resolve::{ResolveOptions, ResolvedGraph};
use crate::source::GlobalCache;
use crate::sync::apply::ApplyResult;
pub use crate::sync::apply::SyncOptions;
use crate::sync::target::{TargetItem, TargetState};
use crate::types::{ContentHash, DestPath, MarsContext, SourceId, SourceName, SourceOrigin};
use crate::validate::ValidationWarning;

// Re-export mutation types for public API compatibility.
pub use crate::sync::mutation::{ConfigMutation, DependencyUpsertChange, apply_config_mutation};

/// Report from a completed sync operation.
#[derive(Debug)]
pub struct SyncReport {
    pub applied: ApplyResult,
    pub pruned: Vec<apply::ActionOutcome>,
    pub diagnostics: Vec<Diagnostic>,
    pub dependency_changes: Vec<DependencyUpsertChange>,
    pub upgrades_available: usize,
    /// Per-target sync outcomes from the target sync phase.
    pub target_outcomes: Vec<crate::target_sync::TargetSyncOutcome>,
    /// Whether this was a dry run (`--diff`). Affects output wording only.
    pub dry_run: bool,
}

impl SyncReport {
    /// Whether the sync produced any unresolved conflicts.
    pub fn has_conflicts(&self) -> bool {
        self.applied
            .outcomes
            .iter()
            .any(|o| matches!(o.action, apply::ActionTaken::Conflicted))
    }
}

/// What a CLI command requests from the sync pipeline.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    /// How to resolve versions.
    pub resolution: ResolutionMode,
    /// Config mutation to apply under flock.
    pub mutation: Option<ConfigMutation>,
    /// Behavior flags.
    pub options: SyncOptions,
}

/// Resolution behavior for the resolver stage.
#[derive(Debug, Clone)]
pub enum ResolutionMode {
    /// Normal sync behavior.
    Normal,
    /// Upgrade behavior (maximize versions), optionally scoped to specific
    /// sources and optionally bumping direct constraints.
    Maximize {
        targets: HashSet<SourceName>,
        bump: bool,
    },
}

// ---------------------------------------------------------------------------
// Pipeline phase structs — typed handoffs between pipeline stages.
// Phase functions consume prior state by value (move semantics, no cloning).
// ---------------------------------------------------------------------------

/// Phase 1: Load and validate configuration under sync lock.
pub(crate) struct LoadedConfig {
    pub config: Config,
    pub local: LocalConfig,
    pub effective: EffectiveConfig,
    pub old_lock: LockFile,
    pub dependency_changes: Vec<DependencyUpsertChange>,
    /// Intentional keepalive — holds the sync file lock for the duration of the pipeline. Dropping this field releases the lock.
    #[allow(dead_code)]
    pub sync_lock: FileLock,
}

/// Phase 2: Resolved dependency graph.
pub(crate) struct ResolvedState {
    pub loaded: LoadedConfig,
    pub graph: ResolvedGraph,
}

/// Phase 3: Desired target state after discovery + filtering.
pub(crate) struct TargetedState {
    pub resolved: ResolvedState,
    pub target: TargetState,
    pub warnings: Vec<ValidationWarning>,
}

/// Phase 4: Diff + plan ready for execution.
pub(crate) struct PlannedState {
    pub targeted: TargetedState,
    pub plan: plan::SyncPlan,
}

/// Phase 5: Applied results.
pub(crate) struct AppliedState {
    pub planned: PlannedState,
    pub applied: ApplyResult,
}

/// Phase 6: Target sync results.
pub(crate) struct SyncedState {
    pub applied: AppliedState,
    pub target_outcomes: Vec<crate::target_sync::TargetSyncOutcome>,
}

/// Execute the unified sync pipeline.
///
/// Orchestrates phase functions, each consuming the prior phase's output struct.
pub fn execute(ctx: &MarsContext, request: &SyncRequest) -> Result<SyncReport, MarsError> {
    validate_request(request)?;
    let mut diag = DiagnosticCollector::new();
    let ir = crate::reader::read(ctx, request, &mut diag)?;
    crate::compiler::compile(ctx, ir, request, &mut diag)
}

// ---------------------------------------------------------------------------
// Phase functions
// ---------------------------------------------------------------------------

/// Phase 1: Acquire sync lock, load config, apply mutations, merge effective config,
/// and load the existing lock file.
pub(crate) fn load_config(
    ctx: &MarsContext,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<LoadedConfig, MarsError> {
    let project_root = &ctx.project_root;
    let mars_dir = project_root.join(".mars");

    std::fs::create_dir_all(mars_dir.join("cache"))?;

    // Acquire sync lock before any config reads/mutations.
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    // Load config under lock (auto-init when mutating and missing).
    let mut config = match crate::config::load(project_root) {
        Ok(config) => config,
        Err(err) if mutation::is_config_not_found(&err) && request.mutation.is_some() => Config {
            settings: Settings::default(),
            ..Config::default()
        },
        Err(err) => return Err(err),
    };

    // Apply config mutation.
    let dependency_changes = if let Some(m) = &request.mutation {
        mutation::apply_mutation(&mut config, m)?
    } else {
        Vec::new()
    };

    // Load/mutate local overrides under the same lock.
    let mut local = crate::config::load_local(project_root)?;
    if let Some(m) = &request.mutation {
        mutation::apply_local_mutation(&mut local, m);
    }

    // Build effective config.
    let (effective, config_diagnostics) =
        crate::config::merge_with_root(config.clone(), local.clone(), project_root)?;
    diag.extend(config_diagnostics);

    // Load existing lock file, routing legacy promotion warnings through sync diagnostics.
    let (old_lock, lock_diagnostics) = crate::lock::load_with_diagnostics(project_root)?;
    diag.extend(lock_diagnostics);

    Ok(LoadedConfig {
        config,
        local,
        effective,
        old_lock,
        dependency_changes,
        sync_lock: _sync_lock,
    })
}

/// Phase 2: Validate upgrade targets, resolve the dependency graph.
pub(crate) fn resolve_graph(
    ctx: &MarsContext,
    mut loaded: LoadedConfig,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<ResolvedState, MarsError> {
    validate_targets(&request.resolution, &loaded.effective)?;

    let cache = GlobalCache::new()?;
    let source_provider = provider::RealSourceProvider {
        cache: &cache,
        project_root: &ctx.project_root,
    };
    let resolve_options = to_resolve_options(&request.resolution, request.options.frozen);
    let graph = crate::resolve::resolve(
        &loaded.effective,
        &source_provider,
        Some(&loaded.old_lock),
        &resolve_options,
        diag,
    )?;

    let bump_entries = planned_bump_entries(&loaded.config, &graph, &request.resolution);
    if !bump_entries.is_empty() {
        let bump_changes = mutation::apply_mutation(
            &mut loaded.config,
            &ConfigMutation::BatchUpsert(bump_entries),
        )?;
        loaded.dependency_changes.extend(bump_changes);
    }

    // Merge model config from dependency tree
    let dep_models = declaration_ordered_dep_models(&graph, &loaded.effective);
    let _ = crate::models::merge_model_config(&loaded.config.models, &dep_models, diag, None);

    Ok(ResolvedState { loaded, graph })
}

/// Phase 3: Build target state, handle collisions, rewrite frontmatter refs, validate.
///
/// `local_items` are pre-discovered by the reader stage; no discovery is
/// performed here so that dest-path assignment remains the only compiler
/// concern for local content.
pub(crate) fn build_target(
    ctx: &MarsContext,
    resolved: ResolvedState,
    local_items: Vec<crate::local_source::LocalDiscoveredItem>,
    _request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<TargetedState, MarsError> {
    // Use .mars/ as the canonical content root for diff/collision checks.
    let mars_dir = ctx.project_root.join(".mars");
    let managed_root = &mars_dir;

    // Build target state from resolved graph.
    let (mut target_state, renames) =
        target::build_with_collisions(&resolved.graph, &resolved.loaded.effective)?;

    let local_source_name: SourceName = SourceOrigin::LocalPackage.to_string().into();
    let local_source_id = SourceId::Path {
        canonical: dunce::canonicalize(&ctx.project_root)
            .unwrap_or_else(|_| ctx.project_root.clone()),
        subpath: None,
    };
    let old_lock_index = LockIndex::new(&resolved.loaded.old_lock);

    for item in local_items {
        let source_path = item.disk_path();
        let is_flat_skill = item.discovered.id.kind == ItemKind::Skill
            && item.discovered.source_path == Path::new(".");
        let source_hash = if is_flat_skill {
            ContentHash::from(hash::compute_skill_hash_filtered(
                &source_path,
                crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL,
            )?)
        } else {
            ContentHash::from(hash::compute_hash(&source_path, item.discovered.id.kind)?)
        };
        let dest_path =
            default_dest_path(item.discovered.id.kind, item.discovered.id.name.as_str());

        if let Some(existing) = target_state.items.shift_remove(&dest_path)
            && existing.source_hash != source_hash
        {
            diag.warn(
                "local-shadow",
                format!(
                    "local {} `{}` shadows dependency `{}` {} `{}`",
                    item.discovered.id.kind,
                    item.discovered.id.name,
                    existing.source_name,
                    existing.id.kind,
                    existing.id.name
                ),
            );
        }

        let disk_path = dest_path.resolve(managed_root);
        if !old_lock_index.contains_dest_path(&dest_path) && disk_path.symlink_metadata().is_ok() {
            diag.warn(
                "unmanaged-collision",
                format!(
                    "local {} `{}` collides with unmanaged path `{}` — leaving existing content untouched",
                    item.discovered.id.kind, item.discovered.id.name, dest_path
                ),
            );
            continue;
        }

        target_state.items.insert(
            dest_path.clone(),
            TargetItem {
                id: ItemId {
                    kind: item.discovered.id.kind,
                    name: item.discovered.id.name.clone(),
                },
                source_name: local_source_name.clone(),
                origin: SourceOrigin::LocalPackage,
                source_id: local_source_id.clone(),
                source_path,
                dest_path,
                source_hash,
                is_flat_skill,
                rewritten_content: None,
            },
        );
    }

    // Handle collisions + rewrite frontmatter refs.
    if !renames.is_empty() {
        let rewrite_warnings =
            target::rewrite_skill_refs(&mut target_state, &renames, &resolved.graph)?;
        for w in &rewrite_warnings {
            diag.warn("rewrite-warning", w.to_string());
        }
    }

    // Validate skill references.
    let warnings = validate_skill_refs(managed_root, &target_state);

    // Prevent managed installs from overwriting unmanaged files.
    let unmanaged_collisions =
        target::check_unmanaged_collisions(managed_root, &resolved.loaded.old_lock, &target_state);
    for collision in &unmanaged_collisions {
        diag.warn(
            "unmanaged-collision",
            format!(
                "source `{}` collides with unmanaged path `{}` — leaving existing content untouched",
                collision.source_name, collision.path
            ),
        );
        target_state.items.shift_remove(&collision.path);
    }

    Ok(TargetedState {
        resolved,
        target: target_state,
        warnings,
    })
}

/// Phase 4: Compute diff, create plan.
pub(crate) fn create_plan(
    ctx: &MarsContext,
    targeted: TargetedState,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<PlannedState, MarsError> {
    // Diff against .mars/ canonical store.
    let mars_dir = ctx.project_root.join(".mars");
    let managed_root = &mars_dir;
    let cache_bases_dir = mars_dir.join("cache").join("bases");

    // Compute diff.
    let sync_diff = diff::compute(
        managed_root,
        &targeted.resolved.loaded.old_lock,
        &targeted.target,
        request.options.force,
    )?;

    if !request.options.force {
        for entry in &sync_diff.items {
            if let diff::DiffEntry::LocalModified { target, .. } = entry {
                diag.warn(
                    "disk-lock-divergent",
                    format!(
                        "{} diverged from mars.lock checksum; preserving local content (run `mars sync --force` or `mars repair` to reset)",
                        target.dest_path
                    ),
                );
            }
        }
    }

    // Create plan.
    let sync_plan = plan::create(&sync_diff, &request.options, &cache_bases_dir, diag);

    Ok(PlannedState {
        targeted,
        plan: sync_plan,
    })
}

/// Check that a frozen sync has no pending changes.
pub(crate) fn check_frozen_gate(planned: &PlannedState) -> Result<(), MarsError> {
    let has_changes = planned.plan.actions.iter().any(|a| {
        !matches!(
            a,
            plan::PlannedAction::Skip { .. } | plan::PlannedAction::KeepLocal { .. }
        )
    });
    if has_changes {
        return Err(MarsError::FrozenViolation {
            message: "lock file would change but --frozen is set".into(),
        });
    }
    Ok(())
}

/// Phase 5: Persist config if mutated, apply plan to .mars/ canonical store.
pub(crate) fn apply_plan(
    ctx: &MarsContext,
    planned: PlannedState,
    request: &SyncRequest,
) -> Result<AppliedState, MarsError> {
    let project_root = &ctx.project_root;
    let mars_dir = project_root.join(".mars");
    let cache_bases_dir = mars_dir.join("cache").join("bases");

    let has_bump_version_changes =
        has_version_changes(&planned.targeted.resolved.loaded.dependency_changes)
            && matches!(
                request.resolution,
                ResolutionMode::Maximize { bump: true, .. }
            );
    let has_mutation = request.mutation.is_some() || has_bump_version_changes;

    // Persist config/local only after validation gate and before apply.
    if has_mutation && !request.options.dry_run {
        match &request.mutation {
            Some(ConfigMutation::SetOverride { .. } | ConfigMutation::ClearOverride { .. }) => {
                crate::config::save_local(project_root, &planned.targeted.resolved.loaded.local)?;
            }
            Some(
                ConfigMutation::UpsertDependency { .. }
                | ConfigMutation::BatchUpsert(..)
                | ConfigMutation::RemoveDependency { .. }
                | ConfigMutation::SetRename { .. },
            ) => {
                crate::config::save(project_root, &planned.targeted.resolved.loaded.config)?;
            }
            None => {
                if has_bump_version_changes {
                    crate::config::save(project_root, &planned.targeted.resolved.loaded.config)?;
                }
            }
        }
    }

    // Apply plan to .mars/ canonical store (D25).
    // Content is written to .mars/agents/ and .mars/skills/, then
    // sync_targets() copies to all managed target directories.
    let applied = apply::execute(&mars_dir, &planned.plan, &request.options, &cache_bases_dir)?;

    Ok(AppliedState { planned, applied })
}

/// Phase 6: Sync managed targets from .mars/ canonical store.
///
/// Copies content from .mars/ to all configured target directories.
/// Non-fatal — target sync errors are recorded as diagnostics.
/// Lock is written regardless of target sync outcome (D21).
pub(crate) fn sync_targets(
    ctx: &MarsContext,
    applied: AppliedState,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> SyncedState {
    if request.options.dry_run {
        return SyncedState {
            applied,
            target_outcomes: Vec::new(),
        };
    }

    let mars_dir = ctx.project_root.join(".mars");
    let targets = applied
        .planned
        .targeted
        .resolved
        .loaded
        .effective
        .settings
        .managed_targets();
    let previous_managed_paths = applied
        .planned
        .targeted
        .resolved
        .loaded
        .old_lock
        .all_output_dest_paths()
        .map(|dest_path| dest_path.to_string())
        .collect::<HashSet<String>>();

    let target_outcomes = crate::target_sync::sync_managed_targets(
        &ctx.project_root,
        &mars_dir,
        &targets,
        &applied.applied.outcomes,
        &previous_managed_paths,
        request.options.force,
        diag,
    );

    SyncedState {
        applied,
        target_outcomes,
    }
}

/// Phase 7: Write lock file, construct SyncReport.
///
/// Lock is written regardless of target sync outcome (D21).
pub(crate) fn finalize(
    ctx: &MarsContext,
    state: SyncedState,
    request: &SyncRequest,
    diag: &mut DiagnosticCollector,
) -> Result<SyncReport, MarsError> {
    let project_root = &ctx.project_root;
    let old_lock = &state.applied.planned.targeted.resolved.loaded.old_lock;
    let graph = &state.applied.planned.targeted.resolved.graph;

    // Write lock file (D21 — regardless of target sync outcome).
    if !request.options.dry_run {
        let new_lock = crate::lock::build(graph, &state.applied.applied, old_lock)?;
        crate::lock::write(project_root, &new_lock)?;

        // Persist dependency-only model aliases so `mars models list` can load
        // deps from cache, then overlay current consumer config without keeping
        // stale consumer aliases from prior syncs.
        let dep_models = declaration_ordered_dep_models(
            graph,
            &state.applied.planned.targeted.resolved.loaded.effective,
        );
        let empty_consumer: indexmap::IndexMap<String, crate::models::ModelAlias> =
            indexmap::IndexMap::new();
        let mut ignored_diag = DiagnosticCollector::new();
        let dep_model_aliases = crate::models::merge_model_config(
            &empty_consumer,
            &dep_models,
            &mut ignored_diag,
            None,
        );

        // Best-effort models cache refresh: ensure the catalog covers any
        // new aliases we're about to persist. Sync never aborts on refresh
        // failure — warn and continue.
        let mars_path = ctx.project_root.join(".mars");
        let ttl = crate::models::load_models_cache_ttl(ctx);
        let mode = crate::models::resolve_refresh_mode(request.options.no_refresh_models);
        match crate::models::ensure_fresh(&mars_path, ttl, mode) {
            Ok((_, crate::models::RefreshOutcome::StaleFallback { reason })) => {
                diag.warn(
                    "models-cache-refresh",
                    format!("using stale models cache: {reason}"),
                );
            }
            Ok((_, crate::models::RefreshOutcome::Offline)) => {}
            Ok(_) => {}
            Err(err) => {
                diag.warn(
                    "models-cache-refresh",
                    format!("failed to refresh models cache: {err}"),
                );
            }
        }

        match serde_json::to_string_pretty(&dep_model_aliases) {
            Ok(json) => {
                let merged_path = ctx.project_root.join(".mars").join("models-merged.json");
                if let Err(err) = crate::fs::atomic_write(&merged_path, json.as_bytes()) {
                    diag.warn(
                        "models-merge-write",
                        format!("failed to write models-merged.json: {err}"),
                    );
                }
            }
            Err(err) => {
                diag.warn(
                    "models-merge-write",
                    format!("failed to serialize merged model aliases: {err}"),
                );
            }
        }
    }

    for w in &state.applied.planned.targeted.warnings {
        match w {
            ValidationWarning::MissingSkill {
                agent,
                skill_name,
                suggestion,
            } => {
                let msg = match suggestion {
                    Some(s) => format!(
                        "agent `{}` references missing skill `{}` (did you mean `{}`?)",
                        agent.name, skill_name, s
                    ),
                    None => {
                        format!(
                            "agent `{}` references missing skill `{}`",
                            agent.name, skill_name
                        )
                    }
                };
                diag.warn("missing-skill", msg);
            }
        }
    }
    let dependency_changes = state
        .applied
        .planned
        .targeted
        .resolved
        .loaded
        .dependency_changes;
    let upgrades_available = if request.options.frozen {
        0
    } else {
        graph
            .nodes
            .values()
            .filter(|node| {
                matches!(
                    (&node.resolved_ref.version, &node.latest_version),
                    (Some(resolved), Some(latest)) if latest > resolved
                )
            })
            .count()
    };

    Ok(SyncReport {
        applied: state.applied.applied,
        pruned: Vec::new(),
        diagnostics: diag.drain(),
        dependency_changes,
        upgrades_available,
        target_outcomes: state.target_outcomes,
        dry_run: request.options.dry_run,
    })
}

fn declaration_ordered_dep_models(
    graph: &ResolvedGraph,
    config: &EffectiveConfig,
) -> Vec<crate::models::ResolvedDepModels> {
    // Declaration positions for direct deps in consumer mars.toml.
    let mut decl_pos: HashMap<SourceName, usize> = HashMap::new();
    for (idx, name) in config.dependencies.keys().enumerate() {
        decl_pos.insert(name.clone(), idx);
    }

    // Propagate declaration position to transitives: a transitive dependency
    // takes the minimum position among all direct dependencies that reach it.
    for (idx, sponsor) in config.dependencies.keys().enumerate() {
        let Some(sponsor_node) = graph.nodes.get(sponsor) else {
            continue;
        };

        let mut queue: VecDeque<SourceName> = sponsor_node.deps.iter().cloned().collect();
        let mut visited: HashSet<SourceName> = HashSet::new();

        while let Some(dep) = queue.pop_front() {
            if !visited.insert(dep.clone()) {
                continue;
            }

            decl_pos
                .entry(dep.clone())
                .and_modify(|pos| *pos = (*pos).min(idx))
                .or_insert(idx);

            if let Some(dep_node) = graph.nodes.get(&dep) {
                queue.extend(dep_node.deps.iter().cloned());
            }
        }
    }

    // Build Kahn structures using dependency edges:
    // dep -> dependent (name depends on dep).
    let mut in_degree: HashMap<SourceName, usize> = HashMap::new();
    let mut adjacency: HashMap<SourceName, Vec<SourceName>> = HashMap::new();

    for name in graph.nodes.keys() {
        in_degree.entry(name.clone()).or_insert(0);
        adjacency.entry(name.clone()).or_default();
    }

    for (name, node) in &graph.nodes {
        for dep in &node.deps {
            if graph.nodes.contains_key(dep) {
                *in_degree.entry(name.clone()).or_insert(0) += 1;
                adjacency.entry(dep.clone()).or_default().push(name.clone());
            }
        }
    }

    let mut ready: BinaryHeap<Reverse<(usize, SourceName)>> = BinaryHeap::new();
    for (name, degree) in &in_degree {
        if *degree == 0 {
            let position = decl_pos.get(name).copied().unwrap_or(usize::MAX);
            ready.push(Reverse((position, name.clone())));
        }
    }

    let mut ordered: Vec<SourceName> = Vec::with_capacity(graph.nodes.len());
    while let Some(Reverse((_, current))) = ready.pop() {
        ordered.push(current.clone());

        if let Some(dependents) = adjacency.get(&current) {
            for dependent in dependents {
                if let Some(degree) = in_degree.get_mut(dependent) {
                    *degree -= 1;
                    if *degree == 0 {
                        let position = decl_pos.get(dependent).copied().unwrap_or(usize::MAX);
                        ready.push(Reverse((position, dependent.clone())));
                    }
                }
            }
        }
    }

    // Graph should already be acyclic from resolver; this keeps behavior
    // deterministic if that invariant is ever violated.
    let ordered_names: Vec<SourceName> = if ordered.len() == graph.nodes.len() {
        ordered
    } else {
        graph.order.clone()
    };

    ordered_names
        .iter()
        .filter_map(|name| {
            let node = graph.nodes.get(name)?;
            let manifest = node.manifest.as_ref()?;
            if manifest.models.is_empty() {
                return None;
            }
            Some(crate::models::ResolvedDepModels {
                source_name: name.to_string(),
                models: manifest.models.clone(),
            })
        })
        .collect()
}

fn default_dest_path(kind: ItemKind, name: &str) -> DestPath {
    match kind {
        ItemKind::Agent => DestPath::from(format!("agents/{name}.md")),
        ItemKind::Skill => DestPath::from(format!("skills/{name}")),
        ItemKind::Hook => DestPath::from(format!("hooks/{name}")),
        ItemKind::McpServer => DestPath::from(format!("mcp/{name}")),
        ItemKind::BootstrapDoc => DestPath::from(format!("bootstrap/{name}")),
    }
}

fn validate_request(request: &SyncRequest) -> Result<(), MarsError> {
    if request.options.frozen && matches!(request.resolution, ResolutionMode::Maximize { .. }) {
        return Err(MarsError::InvalidRequest {
            message:
                "cannot use --frozen with upgrade (frozen locks versions; upgrade maximizes them)"
                    .to_string(),
        });
    }

    if request.options.frozen && request.mutation.is_some() {
        return Err(MarsError::InvalidRequest {
            message:
                "cannot modify config in --frozen mode (config change would require lock update)"
                    .to_string(),
        });
    }

    Ok(())
}

fn validate_targets(
    resolution: &ResolutionMode,
    effective: &EffectiveConfig,
) -> Result<(), MarsError> {
    if let ResolutionMode::Maximize { targets, .. } = resolution {
        for name in targets {
            if !effective.dependencies.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.to_string(),
                    message: format!("dependency `{name}` not found in mars.toml"),
                });
            }
        }
    }

    Ok(())
}

fn to_resolve_options(mode: &ResolutionMode, frozen: bool) -> ResolveOptions {
    match mode {
        ResolutionMode::Normal => ResolveOptions {
            frozen,
            ..ResolveOptions::default()
        },
        ResolutionMode::Maximize { targets, bump } => ResolveOptions {
            maximize: true,
            upgrade_targets: targets.clone(),
            bump_direct_constraints: *bump,
            frozen,
        },
    }
}

fn planned_bump_entries(
    config: &Config,
    graph: &ResolvedGraph,
    mode: &ResolutionMode,
) -> Vec<(SourceName, crate::config::DependencyEntry)> {
    let ResolutionMode::Maximize {
        targets,
        bump: true,
    } = mode
    else {
        return Vec::new();
    };

    config
        .dependencies
        .iter()
        .filter_map(|(name, entry)| {
            if !targets.is_empty() && !targets.contains(name) {
                return None;
            }
            // Only git dependencies with semver-tagged resolution can be bumped.
            entry.url.as_ref()?;
            let node = graph.nodes.get(name)?;
            let resolved_version = node.resolved_ref.version.as_ref()?;
            let resolved_tag = node.resolved_ref.version_tag.as_ref()?;
            if !constraint_needs_bump(entry.version.as_deref(), resolved_version) {
                return None;
            }
            if entry.version.as_deref() == Some(resolved_tag.as_str()) {
                return None;
            }
            let mut bumped = entry.clone();
            bumped.version = Some(resolved_tag.clone());
            Some((name.clone(), bumped))
        })
        .collect()
}

fn constraint_needs_bump(current: Option<&str>, resolved: &semver::Version) -> bool {
    match crate::resolve::parse_version_constraint(current) {
        crate::resolve::VersionConstraint::Semver(req) => !req.matches(resolved),
        crate::resolve::VersionConstraint::Latest
        | crate::resolve::VersionConstraint::RefPin(_) => false,
    }
}

fn has_version_changes(changes: &[DependencyUpsertChange]) -> bool {
    changes
        .iter()
        .any(|change| change.old_version != change.new_version)
}

/// Validate skill references: check that agents' `skills:` frontmatter entries
/// reference skills that exist in the target state.
fn validate_skill_refs(
    install_target: &std::path::Path,
    target: &target::TargetState,
) -> Vec<ValidationWarning> {
    use crate::lock::ItemKind;

    // Collect available skill names
    let available_skills: HashSet<String> = target
        .items
        .values()
        .filter(|item| item.id.kind == ItemKind::Skill)
        .map(|item| item.id.name.to_string())
        .collect();

    // Collect agents with their paths
    let agents: Vec<(String, PathBuf)> = target
        .items
        .values()
        .filter(|item| item.id.kind == ItemKind::Agent)
        .map(|item| {
            let disk_path = item.dest_path.resolve(install_target);
            // If the file exists on disk, use that (may have local edits).
            // Otherwise, use the source path.
            let path = if disk_path.exists() {
                disk_path
            } else {
                item.source_path.clone()
            };
            (item.id.name.to_string(), path)
        })
        .collect();

    crate::validate::check_deps(&agents, &available_skills).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::lock::{ItemKind, LockFile};
    use crate::resolve::{ResolvedGraph, ResolvedNode};
    use indexmap::IndexMap;
    use std::fs;
    use tempfile::TempDir;

    /// Helper to set up a complete sync context with temp dirs.
    struct TestFixture {
        project_root: TempDir,
        managed_root: PathBuf,
        source_trees: Vec<TempDir>,
    }

    impl TestFixture {
        fn new() -> Self {
            let project_root = TempDir::new().unwrap();
            let managed_root = project_root.path().join(".agents");
            // Create .mars/cache directories
            fs::create_dir_all(project_root.path().join(".mars/cache/bases")).unwrap();
            TestFixture {
                project_root,
                managed_root,
                source_trees: Vec::new(),
            }
        }

        fn add_source(&mut self, agents: &[(&str, &str)], skills: &[(&str, &str)]) -> usize {
            let dir = TempDir::new().unwrap();
            if !agents.is_empty() {
                let agents_dir = dir.path().join("agents");
                fs::create_dir_all(&agents_dir).unwrap();
                for (name, content) in agents {
                    fs::write(agents_dir.join(name), content).unwrap();
                }
            }
            if !skills.is_empty() {
                let skills_dir = dir.path().join("skills");
                fs::create_dir_all(&skills_dir).unwrap();
                for (name, content) in skills {
                    let skill_dir = skills_dir.join(name);
                    fs::create_dir_all(&skill_dir).unwrap();
                    fs::write(skill_dir.join("SKILL.md"), content).unwrap();
                }
            }
            self.source_trees.push(dir);
            self.source_trees.len() - 1
        }

        fn project_root(&self) -> &std::path::Path {
            self.project_root.path()
        }

        fn managed_root(&self) -> &std::path::Path {
            &self.managed_root
        }

        fn tree_path(&self, idx: usize) -> PathBuf {
            self.source_trees[idx].path().to_path_buf()
        }
    }

    fn make_graph_config(
        fixture: &TestFixture,
        sources: Vec<(&str, usize, FilterMode)>,
    ) -> (ResolvedGraph, EffectiveConfig) {
        let mut nodes = IndexMap::new();
        let mut order = Vec::new();
        let mut config_dependencies = IndexMap::new();

        for (name, tree_idx, filter) in sources {
            let tree_path = fixture.tree_path(tree_idx);
            nodes.insert(
                name.into(),
                ResolvedNode {
                    source_name: name.into(),
                    source_id: crate::types::SourceId::Path {
                        canonical: tree_path.clone(),
                        subpath: None,
                    },
                    rooted_ref: crate::resolve::RootedSourceRef {
                        checkout_root: tree_path.clone(),
                        package_root: tree_path.clone(),
                    },
                    resolved_ref: crate::source::ResolvedRef {
                        source_name: name.into(),
                        version: None,
                        version_tag: None,
                        commit: None,
                        tree_path: tree_path.clone(),
                    },
                    latest_version: None,
                    manifest: None,
                    deps: vec![],
                },
            );
            order.push(name.into());

            config_dependencies.insert(
                name.into(),
                EffectiveDependency {
                    name: name.into(),
                    id: crate::types::SourceId::Path {
                        canonical: tree_path.clone(),
                        subpath: None,
                    },
                    spec: SourceSpec::Path(tree_path),
                    subpath: None,
                    filter,
                    rename: crate::types::RenameMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }

        (
            ResolvedGraph {
                nodes,
                order,
                filters: std::collections::HashMap::new(),
            },
            EffectiveConfig {
                dependencies: config_dependencies,
                settings: Settings::default(),
            },
        )
    }

    fn path_dependency_entry(path: &std::path::Path) -> DependencyEntry {
        DependencyEntry {
            url: None,
            path: Some(path.to_path_buf()),
            subpath: None,
            version: None,
            filter: FilterConfig::default(),
        }
    }

    fn git_dependency_entry(url: &str, version: &str, filter: FilterConfig) -> DependencyEntry {
        DependencyEntry {
            url: Some(url.into()),
            path: None,
            subpath: None,
            version: Some(version.to_string()),
            filter,
        }
    }

    fn create_sync_plan(
        sync_diff: &diff::SyncDiff,
        options: &SyncOptions,
        cache_bases_dir: &std::path::Path,
    ) -> plan::SyncPlan {
        let mut diag = DiagnosticCollector::new();
        plan::create(sync_diff, options, cache_bases_dir, &mut diag)
    }

    fn graph_with_versions(entries: &[(&str, &str, &str)]) -> ResolvedGraph {
        let mut nodes = IndexMap::new();
        let mut order = Vec::new();
        for (name, url, tag) in entries {
            let version = semver::Version::parse(tag.trim_start_matches('v')).unwrap();
            nodes.insert(
                (*name).into(),
                ResolvedNode {
                    source_name: (*name).into(),
                    source_id: crate::types::SourceId::git(crate::types::SourceUrl::from(*url)),
                    rooted_ref: crate::resolve::RootedSourceRef {
                        checkout_root: PathBuf::from(format!("/tmp/{name}")),
                        package_root: PathBuf::from(format!("/tmp/{name}")),
                    },
                    resolved_ref: crate::source::ResolvedRef {
                        source_name: (*name).into(),
                        version: Some(version),
                        version_tag: Some((*tag).to_string()),
                        commit: Some("abc123".into()),
                        tree_path: PathBuf::from(format!("/tmp/{name}")),
                    },
                    latest_version: None,
                    manifest: None,
                    deps: vec![],
                },
            );
            order.push((*name).into());
        }

        ResolvedGraph {
            nodes,
            order,
            filters: std::collections::HashMap::new(),
        }
    }

    fn model_alias(model: &str) -> crate::models::ModelAlias {
        crate::models::ModelAlias {
            harness: None,
            description: None,
            default_effort: None,
            autocompact: None,
            spec: crate::models::ModelSpec::Pinned {
                model: model.to_string(),
                provider: None,
            },
        }
    }

    fn manifest_with_models(name: &str) -> Manifest {
        let mut models = IndexMap::new();
        models.insert(
            format!("{name}-alias"),
            model_alias(&format!("{name}-model")),
        );
        Manifest {
            package: PackageInfo {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
            },
            dependencies: IndexMap::new(),
            models,
        }
    }

    fn resolved_node(name: &str, deps: &[&str], with_models: bool) -> ResolvedNode {
        let canonical = PathBuf::from(format!("/tmp/{name}"));
        ResolvedNode {
            source_name: name.into(),
            source_id: crate::types::SourceId::Path {
                canonical: canonical.clone(),
                subpath: None,
            },
            rooted_ref: crate::resolve::RootedSourceRef {
                checkout_root: canonical.clone(),
                package_root: canonical.clone(),
            },
            resolved_ref: crate::source::ResolvedRef {
                source_name: name.into(),
                version: None,
                version_tag: None,
                commit: None,
                tree_path: canonical,
            },
            latest_version: None,
            manifest: with_models.then(|| manifest_with_models(name)),
            deps: deps.iter().map(|dep| (*dep).into()).collect(),
        }
    }

    fn effective_config_with_decl_order(names: &[&str]) -> EffectiveConfig {
        let mut dependencies = IndexMap::new();
        for name in names {
            let canonical = PathBuf::from(format!("/tmp/dep-{name}"));
            dependencies.insert(
                (*name).into(),
                EffectiveDependency {
                    name: (*name).into(),
                    id: crate::types::SourceId::Path {
                        canonical: canonical.clone(),
                        subpath: None,
                    },
                    spec: SourceSpec::Path(canonical),
                    subpath: None,
                    filter: FilterMode::All,
                    rename: crate::types::RenameMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }
        EffectiveConfig {
            dependencies,
            settings: Settings::default(),
        }
    }

    fn dep_model_names(models: &[crate::models::ResolvedDepModels]) -> Vec<String> {
        models.iter().map(|m| m.source_name.clone()).collect()
    }

    #[test]
    fn declaration_ordered_dep_models_sibling_order() {
        let mut nodes = IndexMap::new();
        nodes.insert("a".into(), resolved_node("a", &[], true));
        nodes.insert("b".into(), resolved_node("b", &[], true));

        let graph = ResolvedGraph {
            nodes,
            order: vec!["a".into(), "b".into()],
            filters: std::collections::HashMap::new(),
        };
        let config = effective_config_with_decl_order(&["a", "b"]);

        let dep_models = declaration_ordered_dep_models(&graph, &config);
        assert_eq!(dep_model_names(&dep_models), vec!["a", "b"]);
    }

    #[test]
    fn declaration_ordered_dep_models_diamond_uses_minimum_sponsor_position() {
        let mut nodes = IndexMap::new();
        nodes.insert("a".into(), resolved_node("a", &["d"], true));
        nodes.insert("b".into(), resolved_node("b", &["d"], true));
        nodes.insert("d".into(), resolved_node("d", &[], true));

        let graph = ResolvedGraph {
            nodes,
            order: vec!["d".into(), "a".into(), "b".into()],
            filters: std::collections::HashMap::new(),
        };
        let config = effective_config_with_decl_order(&["a", "b"]);

        let dep_models = declaration_ordered_dep_models(&graph, &config);
        assert_eq!(dep_model_names(&dep_models), vec!["d", "a", "b"]);
    }

    #[test]
    fn declaration_ordered_dep_models_transitives_follow_sponsor_declaration_order() {
        let mut nodes = IndexMap::new();
        nodes.insert("a".into(), resolved_node("a", &["d"], false));
        nodes.insert("b".into(), resolved_node("b", &["e"], false));
        nodes.insert("d".into(), resolved_node("d", &[], true));
        nodes.insert("e".into(), resolved_node("e", &[], true));

        let graph = ResolvedGraph {
            nodes,
            order: vec!["d".into(), "e".into(), "a".into(), "b".into()],
            filters: std::collections::HashMap::new(),
        };
        let config = effective_config_with_decl_order(&["a", "b"]);

        let dep_models = declaration_ordered_dep_models(&graph, &config);
        assert_eq!(dep_model_names(&dep_models), vec!["d", "e"]);
    }

    #[test]
    fn declaration_ordered_dep_models_keeps_deps_before_dependents() {
        let mut nodes = IndexMap::new();
        nodes.insert("a".into(), resolved_node("a", &["d"], true));
        nodes.insert("d".into(), resolved_node("d", &[], true));

        let graph = ResolvedGraph {
            nodes,
            order: vec!["d".into(), "a".into()],
            filters: std::collections::HashMap::new(),
        };
        // D is declared after A, but topological ordering must still emit D first.
        let config = effective_config_with_decl_order(&["a", "d"]);

        let dep_models = declaration_ordered_dep_models(&graph, &config);
        assert_eq!(dep_model_names(&dep_models), vec!["d", "a"]);
    }

    #[test]
    fn declaration_ordered_dep_models_is_deterministic() {
        let mut nodes = IndexMap::new();
        nodes.insert("a".into(), resolved_node("a", &["d"], true));
        nodes.insert("b".into(), resolved_node("b", &["e"], true));
        nodes.insert("d".into(), resolved_node("d", &[], true));
        nodes.insert("e".into(), resolved_node("e", &[], true));

        let graph = ResolvedGraph {
            nodes,
            order: vec!["d".into(), "e".into(), "a".into(), "b".into()],
            filters: std::collections::HashMap::new(),
        };
        let config = effective_config_with_decl_order(&["a", "b"]);

        let first = dep_model_names(&declaration_ordered_dep_models(&graph, &config));
        for _ in 0..10 {
            let current = dep_model_names(&declaration_ordered_dep_models(&graph, &config));
            assert_eq!(current, first);
        }
    }

    #[test]
    fn declaration_ordered_dep_models_is_used_by_resolve_graph_and_finalize() {
        let source = include_str!("mod.rs");
        assert!(source.contains("declaration_ordered_dep_models(&graph, &loaded.effective)"));
        assert!(source.contains("&state.applied.planned.targeted.resolved.loaded.effective"));
    }

    #[test]
    fn validate_request_rejects_frozen_with_maximize() {
        let request = SyncRequest {
            resolution: ResolutionMode::Maximize {
                targets: HashSet::new(),
                bump: false,
            },
            mutation: None,
            options: SyncOptions {
                force: false,
                dry_run: false,
                frozen: true,
                no_refresh_models: false,
            },
        };

        let err = validate_request(&request).unwrap_err();
        assert!(matches!(err, MarsError::InvalidRequest { .. }));
        assert!(err.to_string().contains("--frozen"));
    }

    #[test]
    fn validate_request_rejects_frozen_with_mutation() {
        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::RemoveDependency {
                name: "base".into(),
            }),
            options: SyncOptions {
                force: false,
                dry_run: false,
                frozen: true,
                no_refresh_models: false,
            },
        };

        let err = validate_request(&request).unwrap_err();
        assert!(matches!(err, MarsError::InvalidRequest { .. }));
        assert!(err.to_string().contains("cannot modify config"));
    }

    #[test]
    fn planned_bump_entries_bump_all_outdated_pins() {
        let mut config = Config::default();
        config.dependencies.insert(
            "base".into(),
            git_dependency_entry(
                "https://example.com/base.git",
                "v1.0.0",
                FilterConfig::default(),
            ),
        );
        config.dependencies.insert(
            "tools".into(),
            git_dependency_entry(
                "https://example.com/tools.git",
                "v2.0.0",
                FilterConfig::default(),
            ),
        );
        config.dependencies.insert(
            "floating".into(),
            DependencyEntry {
                url: Some("https://example.com/floating.git".into()),
                path: None,
                subpath: None,
                version: None,
                filter: FilterConfig::default(),
            },
        );

        let graph = graph_with_versions(&[
            ("base", "https://example.com/base.git", "v1.2.0"),
            ("tools", "https://example.com/tools.git", "v2.0.0"),
            ("floating", "https://example.com/floating.git", "v3.0.0"),
        ]);

        let mode = ResolutionMode::Maximize {
            targets: HashSet::new(),
            bump: true,
        };
        let entries = planned_bump_entries(&config, &graph, &mode);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SourceName::from("base"));
        assert_eq!(entries[0].1.version.as_deref(), Some("v1.2.0"));
    }

    #[test]
    fn planned_bump_entries_bump_specific_targets_only() {
        let mut config = Config::default();
        config.dependencies.insert(
            "base".into(),
            git_dependency_entry(
                "https://example.com/base.git",
                "v1.0.0",
                FilterConfig::default(),
            ),
        );
        config.dependencies.insert(
            "tools".into(),
            git_dependency_entry(
                "https://example.com/tools.git",
                "v1.0.0",
                FilterConfig::default(),
            ),
        );

        let graph = graph_with_versions(&[
            ("base", "https://example.com/base.git", "v2.0.0"),
            ("tools", "https://example.com/tools.git", "v2.0.0"),
        ]);

        let mode = ResolutionMode::Maximize {
            targets: HashSet::from([SourceName::from("tools")]),
            bump: true,
        };
        let entries = planned_bump_entries(&config, &graph, &mode);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SourceName::from("tools"));
        assert_eq!(entries[0].1.version.as_deref(), Some("v2.0.0"));
    }

    #[test]
    fn planned_bump_entries_noop_when_already_latest() {
        let mut config = Config::default();
        config.dependencies.insert(
            "base".into(),
            git_dependency_entry(
                "https://example.com/base.git",
                "v1.2.0",
                FilterConfig::default(),
            ),
        );

        let graph = graph_with_versions(&[("base", "https://example.com/base.git", "v1.2.0")]);

        let mode = ResolutionMode::Maximize {
            targets: HashSet::new(),
            bump: true,
        };
        let entries = planned_bump_entries(&config, &graph, &mode);
        assert!(entries.is_empty());
    }

    #[test]
    fn planned_bump_entries_preserve_filters_and_renames() {
        let mut rename = crate::types::RenameMap::new();
        rename.insert("coder".into(), "coder-v2".into());

        let mut config = Config::default();
        config.dependencies.insert(
            "base".into(),
            git_dependency_entry(
                "https://example.com/base.git",
                "v1.0.0",
                FilterConfig {
                    agents: Some(vec!["coder".into()]),
                    rename: Some(rename.clone()),
                    ..FilterConfig::default()
                },
            ),
        );

        let graph = graph_with_versions(&[("base", "https://example.com/base.git", "v2.0.0")]);
        let mode = ResolutionMode::Maximize {
            targets: HashSet::new(),
            bump: true,
        };
        let entries = planned_bump_entries(&config, &graph, &mode);
        let mut mutated = config.clone();
        let changes =
            mutation::apply_mutation(&mut mutated, &ConfigMutation::BatchUpsert(entries)).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_version.as_deref(), Some("v1.0.0"));
        assert_eq!(changes[0].new_version.as_deref(), Some("v2.0.0"));

        let dep = &mutated.dependencies["base"];
        assert_eq!(dep.version.as_deref(), Some("v2.0.0"));
        assert_eq!(dep.filter.agents.as_deref(), Some(&["coder".into()][..]));
        assert_eq!(dep.filter.rename.as_ref(), Some(&rename));
    }

    #[test]
    fn execute_auto_inits_config_for_mutation() {
        let project_root = TempDir::new().unwrap();
        let managed_root = project_root.path().join(".agents");
        fs::create_dir_all(project_root.path().join(".mars/cache/bases")).unwrap();
        let source = TempDir::new().unwrap();
        fs::create_dir_all(source.path().join("agents")).unwrap();
        fs::write(source.path().join("agents/coder.md"), "# Coder").unwrap();

        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: path_dependency_entry(source.path()),
            }),
            options: SyncOptions::default(),
        };

        let ctx = MarsContext::for_test(project_root.path().to_path_buf(), managed_root.clone());
        let report = execute(&ctx, &request).unwrap();
        assert!(!report.applied.outcomes.is_empty());
        assert!(project_root.path().join("mars.toml").exists());

        let saved = crate::config::load(project_root.path()).unwrap();
        assert!(saved.dependencies.contains_key("base"));
    }

    #[test]
    fn execute_dry_run_with_mutation_does_not_write_config() {
        let project_root = TempDir::new().unwrap();
        let managed_root = project_root.path().join(".agents");
        fs::create_dir_all(project_root.path().join(".mars/cache/bases")).unwrap();
        crate::config::save(
            project_root.path(),
            &Config {
                dependencies: IndexMap::new(),
                settings: Settings::default(),
                ..Config::default()
            },
        )
        .unwrap();

        let source = TempDir::new().unwrap();
        fs::create_dir_all(source.path().join("agents")).unwrap();
        fs::write(source.path().join("agents/coder.md"), "# Coder").unwrap();

        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: path_dependency_entry(source.path()),
            }),
            options: SyncOptions {
                force: false,
                dry_run: true,
                frozen: false,
                no_refresh_models: false,
            },
        };

        let ctx = MarsContext::for_test(project_root.path().to_path_buf(), managed_root.clone());
        let report = execute(&ctx, &request).unwrap();
        assert!(!report.applied.outcomes.is_empty());

        let saved = crate::config::load(project_root.path()).unwrap();
        assert!(!saved.dependencies.contains_key("base"));
        assert!(!managed_root.join("agents/coder.md").exists());
        assert!(!project_root.path().join("mars.lock").exists());
    }

    // === Integration tests for the pipeline stages ===

    #[test]
    fn full_pipeline_fresh_sync() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(
            &[("coder.md", "# Coder agent")],
            &[("planning", "# Planning skill")],
        );

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // Build target
        let (target, renames) = target::build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 2);

        // Compute diff against empty lock
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();

        // All items should be Add
        assert_eq!(sync_diff.items.len(), 2);
        for entry in &sync_diff.items {
            assert!(matches!(entry, diff::DiffEntry::Add { .. }));
        }

        // Create plan
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        assert_eq!(sync_plan.actions.len(), 2);
        for action in &sync_plan.actions {
            assert!(matches!(action, plan::PlannedAction::Install { .. }));
        }

        // Execute plan
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        assert_eq!(result.outcomes.len(), 2);

        // Verify files were created
        assert!(fixture.managed_root().join("agents/coder.md").exists());
        assert!(
            fixture
                .managed_root()
                .join("skills/planning/SKILL.md")
                .exists()
        );

        // Build lock
        let new_lock = crate::lock::build(&graph, &result, &lock).unwrap();
        assert_eq!(new_lock.items.len(), 2);
        assert!(new_lock.items.contains_key("agent/coder"));
        assert!(new_lock.items.contains_key("skill/planning"));
    }

    #[test]
    fn re_sync_no_changes() {
        let mut fixture = TestFixture::new();
        let content = "# Coder agent";
        let src_idx = fixture.add_source(&[("coder.md", content)], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Second sync with same content
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 =
            diff::compute(fixture.managed_root(), &first_lock, &target2, false).unwrap();

        // All items should be Unchanged
        for entry in &sync_diff2.items {
            assert!(
                matches!(entry, diff::DiffEntry::Unchanged { .. }),
                "expected Unchanged, got {entry:?}"
            );
        }

        let sync_plan2 = create_sync_plan(&sync_diff2, &options, &cache_dir);
        for action in &sync_plan2.actions {
            assert!(matches!(action, plan::PlannedAction::Skip { .. }));
        }
    }

    #[test]
    fn source_update_detects_changes() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Version 1")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Update source content
        let agents_dir = fixture.tree_path(src_idx).join("agents");
        fs::write(agents_dir.join("coder.md"), "# Version 2").unwrap();

        // Rebuild target with updated content
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 =
            diff::compute(fixture.managed_root(), &first_lock, &target2, false).unwrap();

        // Should detect an Update
        assert_eq!(sync_diff2.items.len(), 1);
        assert!(matches!(
            &sync_diff2.items[0],
            diff::DiffEntry::Update { .. }
        ));
    }

    #[test]
    fn local_modification_preserved() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Original")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Locally modify the installed file
        fs::write(
            fixture.managed_root().join("agents/coder.md"),
            "# Locally modified",
        )
        .unwrap();

        // Re-sync (source unchanged)
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 =
            diff::compute(fixture.managed_root(), &first_lock, &target2, false).unwrap();

        // Should detect LocalModified
        assert_eq!(sync_diff2.items.len(), 1);
        assert!(matches!(
            &sync_diff2.items[0],
            diff::DiffEntry::LocalModified { .. }
        ));

        // Plan should KeepLocal
        let sync_plan2 = create_sync_plan(&sync_diff2, &options, &cache_dir);
        assert!(matches!(
            &sync_plan2.actions[0],
            plan::PlannedAction::KeepLocal { .. }
        ));
    }

    #[test]
    fn force_overwrites_local_modifications() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Original")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Locally modify the installed file
        fs::write(
            fixture.managed_root().join("agents/coder.md"),
            "# Locally modified",
        )
        .unwrap();

        // Update source too (triggers conflict)
        let agents_dir = fixture.tree_path(src_idx).join("agents");
        fs::write(agents_dir.join("coder.md"), "# Upstream update").unwrap();

        // Re-sync with --force
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 =
            diff::compute(fixture.managed_root(), &first_lock, &target2, false).unwrap();

        let force_options = SyncOptions {
            force: true,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan2 = create_sync_plan(&sync_diff2, &force_options, &cache_dir);
        assert!(matches!(
            &sync_plan2.actions[0],
            plan::PlannedAction::Overwrite { .. }
        ));

        let result2 = apply::execute(
            fixture.managed_root(),
            &sync_plan2,
            &force_options,
            &cache_dir,
        )
        .unwrap();
        assert!(matches!(
            result2.outcomes[0].action,
            apply::ActionTaken::Updated
        ));

        // File should have upstream content
        let content = fs::read_to_string(fixture.managed_root().join("agents/coder.md")).unwrap();
        assert_eq!(content, "# Upstream update");
    }

    #[test]
    fn orphan_removed_when_source_drops_item() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(
            &[("coder.md", "# Coder"), ("reviewer.md", "# Reviewer")],
            &[],
        );

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync — install both
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        assert!(fixture.managed_root().join("agents/coder.md").exists());
        assert!(fixture.managed_root().join("agents/reviewer.md").exists());

        // Remove reviewer from source
        fs::remove_file(fixture.tree_path(src_idx).join("agents/reviewer.md")).unwrap();

        // Re-sync
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 =
            diff::compute(fixture.managed_root(), &first_lock, &target2, false).unwrap();

        // Should have one Unchanged and one Orphan
        let orphan_count = sync_diff2
            .items
            .iter()
            .filter(|e| matches!(e, diff::DiffEntry::Orphan { .. }))
            .count();
        assert_eq!(orphan_count, 1);

        let sync_plan2 = create_sync_plan(&sync_diff2, &options, &cache_dir);
        let result2 =
            apply::execute(fixture.managed_root(), &sync_plan2, &options, &cache_dir).unwrap();

        // Reviewer should be removed
        assert!(!fixture.managed_root().join("agents/reviewer.md").exists());
        // Coder should still be there
        assert!(fixture.managed_root().join("agents/coder.md").exists());

        // Check remove outcome
        let removed = result2
            .outcomes
            .iter()
            .any(|o| matches!(o.action, apply::ActionTaken::Removed));
        assert!(removed);
    }

    #[test]
    fn dry_run_produces_plan_without_changes() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Coder")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();

        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let dry_options = SyncOptions {
            force: false,
            dry_run: true,
            frozen: false,
            no_refresh_models: false,
        };

        let sync_plan = create_sync_plan(&sync_diff, &dry_options, &cache_dir);
        assert!(!sync_plan.actions.is_empty());

        // Execute in dry-run mode
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &dry_options, &cache_dir).unwrap();
        assert!(!result.outcomes.is_empty());

        // No files should have been created
        assert!(!fixture.managed_root().join("agents/coder.md").exists());
    }

    #[test]
    fn lock_written_after_apply() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Coder")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // Full pipeline minus actual sync() (which needs real config files)
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();

        let new_lock = crate::lock::build(&graph, &result, &lock).unwrap();
        crate::lock::write(fixture.project_root(), &new_lock).unwrap();

        // Verify lock file exists and is valid
        let reloaded = crate::lock::load(fixture.project_root()).unwrap();
        assert_eq!(reloaded.items.len(), 1);
        assert!(reloaded.items.contains_key("agent/coder"));

        let item = &reloaded.items["agent/coder"];
        assert_eq!(item.kind, ItemKind::Agent);
        assert!(!item.source_checksum.is_empty());
        assert!(!item.outputs[0].installed_checksum.is_empty());
    }

    #[test]
    fn two_sources_no_collision() {
        let mut fixture = TestFixture::new();
        let src_a = fixture.add_source(&[("coder.md", "# Coder from A")], &[]);
        let src_b = fixture.add_source(&[("reviewer.md", "# Reviewer from B")], &[]);

        let (graph, config) = make_graph_config(
            &fixture,
            vec![
                ("source-a", src_a, FilterMode::All),
                ("source-b", src_b, FilterMode::All),
            ],
        );

        let (target, renames) = target::build_with_collisions(&graph, &config).unwrap();
        assert!(renames.is_empty());
        assert_eq!(target.items.len(), 2);

        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.managed_root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.project_root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        };
        let sync_plan = create_sync_plan(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();

        assert!(fixture.managed_root().join("agents/coder.md").exists());
        assert!(fixture.managed_root().join("agents/reviewer.md").exists());
        assert_eq!(result.outcomes.len(), 2);
    }

    // === Tests for OnlySkills / OnlyAgents filter in pipeline ===

    #[test]
    fn pipeline_only_skills_filter() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(
            &[("coder.md", "# Coder agent")],
            &[("planning", "# Planning skill")],
        );

        let (graph, config) =
            make_graph_config(&fixture, vec![("base", src_idx, FilterMode::OnlySkills)]);

        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        // Should only have the skill, not the agent
        assert_eq!(target.items.len(), 1);
        assert!(target.items.contains_key("skills/planning"));
    }

    #[test]
    fn pipeline_only_agents_filter() {
        let mut fixture = TestFixture::new();
        // Agent with a skill dependency in frontmatter
        let agent_content = "---\nskills:\n  - planning\n---\n# Coder agent";
        let src_idx = fixture.add_source(
            &[("coder.md", agent_content)],
            &[
                ("planning", "# Planning skill"),
                ("standalone", "# Standalone skill"),
            ],
        );

        let (graph, config) =
            make_graph_config(&fixture, vec![("base", src_idx, FilterMode::OnlyAgents)]);

        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        // Should have the agent + its transitive skill dep, but NOT standalone
        assert_eq!(target.items.len(), 2);
        assert!(target.items.contains_key("agents/coder.md"));
        assert!(target.items.contains_key("skills/planning"));
        assert!(!target.items.contains_key("skills/standalone"));
    }

    #[test]
    fn pipeline_only_agents_no_agents_source() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[], &[("planning", "# Planning skill")]);

        let (graph, config) =
            make_graph_config(&fixture, vec![("base", src_idx, FilterMode::OnlyAgents)]);

        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        // No agents means nothing gets installed
        assert_eq!(target.items.len(), 0);
    }
}
