pub mod apply;
pub mod diff;
pub mod plan;
pub mod target;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::{
    Config, DependencyEntry, EffectiveConfig, FilterConfig, LocalConfig, Manifest, OverrideEntry,
    Settings,
};
use crate::error::{ConfigError, MarsError};
use crate::hash;
use crate::resolve::{ManifestReader, ResolveOptions, SourceFetcher, VersionLister};
use crate::source::{self, AvailableVersion, GlobalCache, ResolvedRef};
use crate::sync::apply::ApplyResult;
pub use crate::sync::apply::SyncOptions;
use crate::types::{CommitHash, ContentHash, ItemName, MarsContext, SourceName};
use crate::validate::ValidationWarning;

/// Report from a completed sync operation.
#[derive(Debug)]
pub struct SyncReport {
    pub applied: ApplyResult,
    pub pruned: Vec<apply::ActionOutcome>,
    pub warnings: Vec<ValidationWarning>,
    pub dependency_changes: Vec<DependencyUpsertChange>,
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
    /// Upgrade behavior (maximize versions), optionally scoped to specific sources.
    Maximize { targets: HashSet<SourceName> },
}

/// Config mutation to apply atomically under flock.
#[derive(Debug, Clone)]
pub enum ConfigMutation {
    /// Add or update a dependency in mars.toml.
    UpsertDependency {
        name: SourceName,
        entry: DependencyEntry,
    },
    /// Add or update multiple dependencies in mars.toml atomically under one sync lock.
    BatchUpsert(Vec<(SourceName, DependencyEntry)>),
    /// Remove a dependency from mars.toml.
    RemoveDependency { name: SourceName },
    /// Add or update an override in mars.local.toml.
    SetOverride {
        source_name: SourceName,
        local_path: PathBuf,
    },
    /// Remove an override from mars.local.toml.
    ClearOverride { source_name: SourceName },
    /// Set or update a rename mapping for one managed item.
    SetRename {
        source_name: SourceName,
        from: String,
        to: String,
    },
}

/// Metadata captured when `UpsertDependency` mutates an existing/new dependency.
#[derive(Debug, Clone)]
pub struct DependencyUpsertChange {
    pub name: SourceName,
    pub already_exists: bool,
    pub old_filter: Option<FilterConfig>,
    pub new_filter: FilterConfig,
}

/// Link-specific config mutations. Separate type from ConfigMutation
/// to enforce that only link operations use the lightweight (no-sync) mutation path.
#[derive(Debug, Clone)]
pub enum LinkMutation {
    /// Add a link target to settings.links (idempotent).
    Set { target: String },
    /// Remove a link target from settings.links.
    Clear { target: String },
}

/// Apply a link mutation under sync lock, without running the full sync pipeline.
/// Only for settings.links changes — use sync::execute for source mutations.
pub fn mutate_link_config(ctx: &MarsContext, mutation: &LinkMutation) -> Result<(), MarsError> {
    let mars_dir = ctx.project_root.join(".mars");
    std::fs::create_dir_all(&mars_dir)?;
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut config = crate::config::load(&ctx.project_root)?;
    match mutation {
        LinkMutation::Set { target } => {
            if !config.settings.links.contains(target) {
                config.settings.links.push(target.clone());
            }
        }
        LinkMutation::Clear { target } => {
            config.settings.links.retain(|l| l != target);
        }
    }
    crate::config::save(&ctx.project_root, &config)?;

    Ok(())
}

/// Execute the unified sync pipeline.
pub fn execute(ctx: &MarsContext, request: &SyncRequest) -> Result<SyncReport, MarsError> {
    let project_root = &ctx.project_root;
    let managed_root = &ctx.managed_root;
    let mars_dir = project_root.join(".mars");

    validate_request(request)?;

    std::fs::create_dir_all(mars_dir.join("cache"))?;

    // Step 1: Acquire sync lock before any config reads/mutations.
    let lock_path = mars_dir.join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    // Step 2: Load config under lock (auto-init when mutating and missing).
    let mut config = match crate::config::load(project_root) {
        Ok(config) => config,
        Err(err) if is_config_not_found(&err) && request.mutation.is_some() => Config {
            settings: Settings::default(),
            ..Config::default()
        },
        Err(err) => return Err(err),
    };

    // Step 3: Apply config mutation.
    let has_mutation = request.mutation.is_some();
    let dependency_changes = if let Some(mutation) = &request.mutation {
        apply_mutation(&mut config, mutation)?
    } else {
        Vec::new()
    };

    // Step 4: Load/mutate local overrides under the same lock.
    let mut local = crate::config::load_local(project_root)?;
    if let Some(mutation) = &request.mutation {
        apply_local_mutation(&mut local, mutation);
    }

    // Step 4b: Build effective config.
    let effective = crate::config::merge_with_root(config.clone(), local.clone(), project_root)?;

    // Step 5: Validate upgrade targets exist.
    validate_targets(&request.resolution, &effective)?;

    // Step 6: Load existing lock file.
    let old_lock = crate::lock::load(project_root)?;

    // Step 7: Resolve dependency graph.
    let cache = GlobalCache::new()?;
    let provider = RealSourceProvider {
        cache: &cache,
        project_root,
    };
    let resolve_options = to_resolve_options(&request.resolution, request.options.frozen);
    let graph = crate::resolve::resolve(&effective, &provider, Some(&old_lock), &resolve_options)?;

    // Step 8: Build target state.
    let (mut target_state, renames) = target::build_with_collisions(&graph, &effective)?;

    // Step 9: Handle collisions + rewrite frontmatter refs.
    if !renames.is_empty() {
        let rewrite_warnings = target::rewrite_skill_refs(&mut target_state, &renames, &graph)?;
        for w in &rewrite_warnings {
            eprintln!("{w}");
        }
    }

    // Step 10: Validate skill references.
    let warnings = validate_skill_refs(managed_root, &target_state);

    // Step 11: Prevent managed installs from overwriting unmanaged files.
    let unmanaged_collisions =
        target::check_unmanaged_collisions(managed_root, &old_lock, &target_state);
    for collision in &unmanaged_collisions {
        eprintln!(
            "warning: source `{}` collides with unmanaged path `{}` — leaving existing content untouched",
            collision.source_name, collision.path
        );
        target_state.items.shift_remove(&collision.path);
    }

    // Step 12: Compute diff.
    let sync_diff = diff::compute(
        managed_root,
        &old_lock,
        &target_state,
        request.options.force,
    )?;

    // Step 13: Create plan.
    let cache_bases_dir = mars_dir.join("cache").join("bases");
    let mut sync_plan = plan::create(&sync_diff, &request.options, &cache_bases_dir);
    let mut skipped_self_dests: HashSet<crate::types::DestPath> = HashSet::new();

    // Step 13b: Inject local package symlinks into plan.
    if config.package.is_some() {
        let self_items = discover_local_items(project_root)?;

        // Collision check: local items shadow external items
        for item in &self_items {
            if target_state.items.contains_key(&item.dest_rel) {
                let existing = &target_state.items[&item.dest_rel];
                eprintln!(
                    "warning: local {} `{}` shadows dependency `{}` {} `{}`",
                    item.kind, item.name, existing.source_name, existing.id.kind, existing.id.name
                );
                // Remove external item from plan (it will be replaced by symlink)
                let dest_rel = item.dest_rel.clone();
                sync_plan
                    .actions
                    .retain(|a| !action_matches_dest(a, &dest_rel));
                target_state.items.shift_remove(&item.dest_rel);
            }
        }

        // Inject symlink actions for items that need updating
        for item in &self_items {
            let dest = managed_root.join(item.dest_rel.as_path());
            if !old_lock.items.contains_key(&item.dest_rel) && dest.symlink_metadata().is_ok() {
                eprintln!(
                    "warning: local {} `{}` collides with unmanaged path `{}` — leaving existing content untouched",
                    item.kind, item.name, item.dest_rel
                );
                skipped_self_dests.insert(item.dest_rel.clone());
                continue;
            }
            let needs_update = match dest.symlink_metadata() {
                Ok(meta) if meta.file_type().is_symlink() => {
                    let current_target = std::fs::read_link(&dest).ok();
                    let from_dir = dest.parent().unwrap();
                    let expected = pathdiff::diff_paths(&item.source_path, from_dir)
                        .unwrap_or_else(|| item.source_path.clone());
                    current_target.as_deref() != Some(expected.as_path())
                }
                Ok(_) => true,  // exists but not a symlink — replace
                Err(_) => true, // doesn't exist — create
            };
            if needs_update {
                sync_plan.actions.push(plan::PlannedAction::Symlink {
                    source_abs: item.source_path.clone(),
                    dest_rel: item.dest_rel.clone(),
                    kind: item.kind,
                    name: item.name.clone(),
                });
            }
        }

        // Prune old _self entries from lock that are no longer present
        let self_dest_set: std::collections::HashSet<_> =
            self_items.iter().map(|i| &i.dest_rel).collect();
        for (dest_path, locked_item) in &old_lock.items {
            if locked_item.source.as_ref() == "_self" && !self_dest_set.contains(dest_path) {
                sync_plan.actions.push(plan::PlannedAction::Remove {
                    locked: locked_item.clone(),
                });
            }
        }
    } else {
        // No [package] — prune any stale _self entries from lock
        for (_, locked_item) in &old_lock.items {
            if locked_item.source.as_ref() == "_self" {
                sync_plan.actions.push(plan::PlannedAction::Remove {
                    locked: locked_item.clone(),
                });
            }
        }
    }

    // Step 13c: Remove any orphan-removal actions targeting _self items.
    // The diff engine (step 12) doesn't know about _self items, so it marks
    // old _self lock entries as orphans. We handle _self lifecycle explicitly
    // above (inject symlinks + explicit prune), so strip the diff engine's
    // Remove actions for _self items to prevent double-removal.
    sync_plan.actions.retain(|action| {
        if let plan::PlannedAction::Remove { locked } = action {
            locked.source.as_ref() != "_self"
        } else {
            true
        }
    });

    // Step 14: Frozen gate.
    if request.options.frozen {
        let has_changes = sync_plan.actions.iter().any(|a| {
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
    }

    // Step 15: Persist config/local only after validation gate and before apply.
    if has_mutation && !request.options.dry_run {
        match &request.mutation {
            Some(ConfigMutation::SetOverride { .. } | ConfigMutation::ClearOverride { .. }) => {
                crate::config::save_local(project_root, &local)?;
            }
            Some(
                ConfigMutation::UpsertDependency { .. }
                | ConfigMutation::BatchUpsert(..)
                | ConfigMutation::RemoveDependency { .. }
                | ConfigMutation::SetRename { .. },
            ) => {
                crate::config::save(project_root, &config)?;
            }
            None => {}
        }
    }

    // Step 16: Apply plan.
    let applied = apply::execute(managed_root, &sync_plan, &request.options, &cache_bases_dir)?;
    let pruned = Vec::new();

    // Step 17: Write lock file.
    if !request.options.dry_run {
        let self_lock_items = if config.package.is_some() {
            let self_items = discover_local_items(project_root)?;
            let filtered: Vec<_> = self_items
                .into_iter()
                .filter(|item| !skipped_self_dests.contains(&item.dest_rel))
                .collect();
            build_self_lock_items(&filtered)?
        } else {
            Vec::new()
        };
        let self_items_for_lock =
            (!self_lock_items.is_empty()).then_some(self_lock_items.as_slice());
        let new_lock = crate::lock::build(&graph, &applied, &old_lock, self_items_for_lock)?;
        crate::lock::write(project_root, &new_lock)?;
    }

    Ok(SyncReport {
        applied,
        pruned,
        warnings,
        dependency_changes,
        dry_run: request.options.dry_run,
    })
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

fn is_config_not_found(error: &MarsError) -> bool {
    matches!(error, MarsError::Config(ConfigError::NotFound { .. }))
}

/// Apply a config mutation to the in-memory config.
///
/// Public so that CLI commands can batch mutations before triggering sync.
pub fn apply_config_mutation(
    config: &mut Config,
    mutation: &ConfigMutation,
) -> Result<(), MarsError> {
    apply_mutation(config, mutation).map(|_| ())
}

fn apply_mutation(
    config: &mut Config,
    mutation: &ConfigMutation,
) -> Result<Vec<DependencyUpsertChange>, MarsError> {
    match mutation {
        ConfigMutation::UpsertDependency { name, entry } => {
            Ok(vec![apply_dependency_upsert(config, name, entry)])
        }
        ConfigMutation::BatchUpsert(entries) => {
            let mut changes = Vec::with_capacity(entries.len());
            for (name, entry) in entries {
                changes.push(apply_dependency_upsert(config, name, entry));
            }
            Ok(changes)
        }
        ConfigMutation::RemoveDependency { name } => {
            if !config.dependencies.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.to_string(),
                    message: format!("dependency `{name}` not found in mars.toml"),
                });
            }
            config.dependencies.shift_remove(name);
            Ok(Vec::new())
        }
        ConfigMutation::SetOverride { source_name, .. } => {
            if !config.dependencies.contains_key(source_name) {
                return Err(MarsError::Source {
                    source_name: source_name.to_string(),
                    message: format!("dependency `{source_name}` not found in mars.toml"),
                });
            }
            Ok(Vec::new())
        }
        ConfigMutation::SetRename {
            source_name,
            from,
            to,
        } => {
            let dep =
                config
                    .dependencies
                    .get_mut(source_name)
                    .ok_or_else(|| MarsError::Source {
                        source_name: source_name.to_string(),
                        message: format!("dependency `{source_name}` not found in mars.toml"),
                    })?;
            let rename_map = dep
                .filter
                .rename
                .get_or_insert_with(crate::types::RenameMap::new);
            rename_map.insert(ItemName::from(from.as_str()), ItemName::from(to.as_str()));
            Ok(Vec::new())
        }
        ConfigMutation::ClearOverride { .. } => Ok(Vec::new()),
    }
}

fn apply_local_mutation(local: &mut LocalConfig, mutation: &ConfigMutation) {
    match mutation {
        ConfigMutation::SetOverride {
            source_name,
            local_path,
        } => {
            local.overrides.insert(
                source_name.clone(),
                OverrideEntry {
                    path: local_path.clone(),
                },
            );
        }
        ConfigMutation::ClearOverride { source_name } => {
            local.overrides.shift_remove(source_name);
        }
        ConfigMutation::UpsertDependency { .. }
        | ConfigMutation::BatchUpsert(..)
        | ConfigMutation::RemoveDependency { .. }
        | ConfigMutation::SetRename { .. } => {}
    }
}

fn apply_dependency_upsert(
    config: &mut Config,
    name: &SourceName,
    entry: &DependencyEntry,
) -> DependencyUpsertChange {
    if let Some(existing) = config.dependencies.get_mut(name) {
        let old_filter = existing.filter.clone();

        // Merge: update location fields, preserve user customizations
        existing.url = entry.url.clone();
        existing.path = entry.path.clone();
        existing.version = entry.version.clone();
        // Atomic filter replacement: when any filter field is set on the
        // incoming entry, replace the entire filter config (minus rename).
        // This prevents mixed-mode states like agents + only_skills.
        // When no filter flags are provided (e.g., version bump), preserve existing.
        if entry.filter.has_any_filter() {
            let rename = existing.filter.rename.take();
            existing.filter = entry.filter.clone();
            // Preserve rename — those are set via `mars rename`, not `mars add`
            existing.filter.rename = rename;
        }
        // Never overwrite rename rules from add — those are set via `mars rename`

        DependencyUpsertChange {
            name: name.clone(),
            already_exists: true,
            old_filter: Some(old_filter),
            new_filter: existing.filter.clone(),
        }
    } else {
        config.dependencies.insert(name.clone(), entry.clone());
        DependencyUpsertChange {
            name: name.clone(),
            already_exists: false,
            old_filter: None,
            new_filter: entry.filter.clone(),
        }
    }
}

fn validate_targets(
    resolution: &ResolutionMode,
    effective: &EffectiveConfig,
) -> Result<(), MarsError> {
    if let ResolutionMode::Maximize { targets } = resolution {
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
        ResolutionMode::Maximize { targets } => ResolveOptions {
            maximize: true,
            upgrade_targets: targets.clone(),
            frozen,
        },
    }
}

/// Real source provider that delegates to the source module.
///
/// Implements the SourceProvider trait so the resolver can fetch sources
/// and read manifests through a uniform interface.
struct RealSourceProvider<'a> {
    cache: &'a GlobalCache,
    project_root: &'a Path,
}

impl VersionLister for RealSourceProvider<'_> {
    fn list_versions(
        &self,
        url: &crate::types::SourceUrl,
    ) -> Result<Vec<AvailableVersion>, MarsError> {
        source::list_versions(url, self.cache)
    }
}

impl SourceFetcher for RealSourceProvider<'_> {
    fn fetch_git_version(
        &self,
        url: &crate::types::SourceUrl,
        version: &AvailableVersion,
        source_name: &str,
        preferred_commit: Option<&str>,
    ) -> Result<ResolvedRef, MarsError> {
        let fetch_options = source::git::FetchOptions {
            preferred_commit: preferred_commit.map(CommitHash::from),
        };
        source::git::fetch(
            url.as_ref(),
            Some(&version.tag),
            source_name,
            self.cache,
            &fetch_options,
        )
    }

    fn fetch_git_ref(
        &self,
        url: &crate::types::SourceUrl,
        ref_name: &str,
        source_name: &str,
        preferred_commit: Option<&str>,
    ) -> Result<ResolvedRef, MarsError> {
        let fetch_options = source::git::FetchOptions {
            preferred_commit: preferred_commit.map(CommitHash::from),
        };
        source::git::fetch(
            url.as_ref(),
            Some(ref_name),
            source_name,
            self.cache,
            &fetch_options,
        )
    }

    fn fetch_path(&self, path: &Path, source_name: &str) -> Result<ResolvedRef, MarsError> {
        source::path::fetch_path(path, self.project_root, source_name)
    }
}

impl ManifestReader for RealSourceProvider<'_> {
    fn read_manifest(&self, source_tree: &Path) -> Result<Option<Manifest>, MarsError> {
        crate::config::load_manifest(source_tree)
    }
}

/// Validate skill references: check that agents' `skills:` frontmatter entries
/// reference skills that exist in the target state.
fn validate_skill_refs(
    install_target: &Path,
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
            let disk_path = install_target.join(&item.dest_path);
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

/// A local package item discovered under the project root.
struct LocalItem {
    kind: crate::lock::ItemKind,
    name: ItemName,
    /// Absolute path to source — for agents, the .md file; for skills, the directory.
    source_path: PathBuf,
    /// Relative destination under managed root.
    dest_rel: crate::types::DestPath,
}

/// Discover local package items (agents and skills) at the project root.
///
/// Called when `[package]` is present in `mars.toml`. Scans:
/// - `project_root/agents/*.md` → agent items
/// - `project_root/skills/*/` (directories containing SKILL.md) → skill items
fn discover_local_items(project_root: &Path) -> Result<Vec<LocalItem>, MarsError> {
    use crate::lock::ItemKind;
    let mut items = Vec::new();

    // Discover agents
    let agents_dir = project_root.join("agents");
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") && path.is_file() {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                items.push(LocalItem {
                    kind: ItemKind::Agent,
                    name: ItemName::from(name.as_str()),
                    source_path: path.canonicalize().unwrap_or(path.clone()),
                    dest_rel: format!("agents/{}.md", name).into(),
                });
            }
        }
    }

    // Discover skills
    let skills_dir = project_root.join("skills");
    if skills_dir.is_dir() {
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").exists() {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                items.push(LocalItem {
                    kind: ItemKind::Skill,
                    name: ItemName::from(name.as_str()),
                    source_path: path.canonicalize().unwrap_or(path.clone()),
                    dest_rel: format!("skills/{}", name).into(),
                });
            }
        }
    }

    Ok(items)
}

fn build_self_lock_items(items: &[LocalItem]) -> Result<Vec<crate::lock::SelfLockItem>, MarsError> {
    let mut lock_items = Vec::with_capacity(items.len());
    for item in items {
        let source_checksum = ContentHash::from(hash::compute_hash(&item.source_path, item.kind)?);
        lock_items.push(crate::lock::SelfLockItem {
            dest_path: item.dest_rel.clone(),
            kind: item.kind,
            source_checksum,
        });
    }
    Ok(lock_items)
}

/// Check if a planned action targets a specific destination path.
fn action_matches_dest(action: &plan::PlannedAction, dest: &crate::types::DestPath) -> bool {
    match action {
        plan::PlannedAction::Install { target } | plan::PlannedAction::Overwrite { target } => {
            &target.dest_path == dest
        }
        plan::PlannedAction::Skip { dest_path, .. }
        | plan::PlannedAction::KeepLocal { dest_path, .. } => dest_path == dest,
        plan::PlannedAction::Merge { target, .. } => &target.dest_path == dest,
        plan::PlannedAction::Remove { locked } => &locked.dest_path == dest,
        plan::PlannedAction::Symlink { dest_rel, .. } => dest_rel == dest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::lock::{ItemKind, LockFile};
    use crate::resolve::{ResolvedGraph, ResolvedNode};
    use crate::source::ResolvedRef;
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

        fn project_root(&self) -> &Path {
            self.project_root.path()
        }

        fn managed_root(&self) -> &Path {
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
                    },
                    resolved_ref: ResolvedRef {
                        source_name: name.into(),
                        version: None,
                        version_tag: None,
                        commit: None,
                        tree_path: tree_path.clone(),
                    },
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
                    },
                    spec: SourceSpec::Path(tree_path),
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
                id_index: std::collections::HashMap::new(),
            },
            EffectiveConfig {
                dependencies: config_dependencies,
                settings: Settings::default(),
            },
        )
    }

    fn path_dependency_entry(path: &Path) -> DependencyEntry {
        DependencyEntry {
            url: None,
            path: Some(path.to_path_buf()),
            version: None,
            filter: FilterConfig::default(),
        }
    }

    #[test]
    fn validate_request_rejects_frozen_with_maximize() {
        let request = SyncRequest {
            resolution: ResolutionMode::Maximize {
                targets: HashSet::new(),
            },
            mutation: None,
            options: SyncOptions {
                force: false,
                dry_run: false,
                frozen: true,
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
            },
        };

        let err = validate_request(&request).unwrap_err();
        assert!(matches!(err, MarsError::InvalidRequest { .. }));
        assert!(err.to_string().contains("cannot modify config"));
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
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
        let new_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();
        assert_eq!(new_lock.items.len(), 2);
        assert!(new_lock.items.contains_key("agents/coder.md"));
        assert!(new_lock.items.contains_key("skills/planning"));
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();

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

        let sync_plan2 = plan::create(&sync_diff2, &options, &cache_dir);
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();

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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();

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
        let sync_plan2 = plan::create(&sync_diff2, &options, &cache_dir);
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();

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
        };
        let sync_plan2 = plan::create(&sync_diff2, &force_options, &cache_dir);
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();

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

        let sync_plan2 = plan::create(&sync_diff2, &options, &cache_dir);
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
        };

        let sync_plan = plan::create(&sync_diff, &dry_options, &cache_dir);
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();

        let new_lock = crate::lock::build(&graph, &result, &lock, None).unwrap();
        crate::lock::write(fixture.project_root(), &new_lock).unwrap();

        // Verify lock file exists and is valid
        let reloaded = crate::lock::load(fixture.project_root()).unwrap();
        assert_eq!(reloaded.items.len(), 1);
        assert!(reloaded.items.contains_key("agents/coder.md"));

        let item = &reloaded.items["agents/coder.md"];
        assert_eq!(item.kind, ItemKind::Agent);
        assert!(!item.source_checksum.is_empty());
        assert!(!item.installed_checksum.is_empty());
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
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result =
            apply::execute(fixture.managed_root(), &sync_plan, &options, &cache_dir).unwrap();

        assert!(fixture.managed_root().join("agents/coder.md").exists());
        assert!(fixture.managed_root().join("agents/reviewer.md").exists());
        assert_eq!(result.outcomes.len(), 2);
    }

    // === Tests for atomic filter replacement in apply_mutation ===

    #[test]
    fn apply_mutation_atomic_filter_replacement() {
        let mut config = Config::default();
        // First add with agents filter
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["reviewer".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();
        assert!(config.dependencies["base"].filter.agents.is_some());

        // Re-add with only_skills — should atomically replace, clearing agents
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert!(dep.filter.only_skills);
        assert!(
            dep.filter.agents.is_none(),
            "agents should be cleared by atomic replacement"
        );
    }

    #[test]
    fn apply_mutation_preserves_filters_on_version_bump() {
        let mut config = Config::default();
        // Add with agents filter
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["coder".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        // Re-add with no filter (version bump only)
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v2".into()),
            filter: FilterConfig::default(),
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert_eq!(dep.version.as_deref(), Some("v2"));
        assert_eq!(
            dep.filter.agents.as_deref(),
            Some(&["coder".into()][..]),
            "agents filter should be preserved on version bump"
        );
    }

    #[test]
    fn apply_mutation_preserves_rename_on_filter_change() {
        let mut config = Config::default();
        let mut rename_map = crate::types::RenameMap::new();
        rename_map.insert("old".into(), "new".into());

        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: None,
            filter: FilterConfig {
                agents: Some(vec!["coder".into()]),
                rename: Some(rename_map),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        // Re-add with different filter — rename should be preserved
        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: None,
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        let dep = &config.dependencies["base"];
        assert!(dep.filter.only_skills);
        assert!(dep.filter.agents.is_none());
        assert!(
            dep.filter.rename.is_some(),
            "rename should be preserved across filter changes"
        );
        assert_eq!(
            dep.filter.rename.as_ref().unwrap().get("old").unwrap(),
            "new"
        );
    }

    #[test]
    fn apply_mutation_batch_upsert_applies_all_entries() {
        let mut config = Config::default();
        let batch = vec![
            (
                "base".into(),
                DependencyEntry {
                    url: Some("https://github.com/org/base.git".into()),
                    path: None,
                    version: Some("v1".into()),
                    filter: FilterConfig::default(),
                },
            ),
            (
                "workflow".into(),
                DependencyEntry {
                    url: Some("https://github.com/org/workflow.git".into()),
                    path: None,
                    version: Some("v2".into()),
                    filter: FilterConfig::default(),
                },
            ),
        ];

        let changes = apply_mutation(&mut config, &ConfigMutation::BatchUpsert(batch)).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(config.dependencies.contains_key("base"));
        assert!(config.dependencies.contains_key("workflow"));
    }

    #[test]
    fn apply_mutation_returns_old_and_new_filters_for_readd() {
        let mut config = Config::default();
        let entry1 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v1".into()),
            filter: FilterConfig {
                agents: Some(vec!["reviewer".into()]),
                ..FilterConfig::default()
            },
        };
        apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry1,
            },
        )
        .unwrap();

        let entry2 = DependencyEntry {
            url: Some("https://github.com/org/base.git".into()),
            path: None,
            version: Some("v2".into()),
            filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        let changes = apply_mutation(
            &mut config,
            &ConfigMutation::UpsertDependency {
                name: "base".into(),
                entry: entry2,
            },
        )
        .unwrap();

        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert!(change.already_exists);
        assert_eq!(change.name, "base");
        assert_eq!(
            change.old_filter.as_ref().and_then(|f| f.agents.as_deref()),
            Some(&["reviewer".into()][..])
        );
        assert!(change.new_filter.only_skills);
        assert!(change.new_filter.agents.is_none());
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
