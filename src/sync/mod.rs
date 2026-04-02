pub mod apply;
pub mod diff;
pub mod plan;
pub mod target;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::config::{Config, EffectiveConfig, LocalConfig, OverrideEntry, Settings, SourceEntry};
use crate::error::{ConfigError, MarsError};
use crate::resolve::{ManifestReader, ResolveOptions, SourceFetcher, VersionLister};
use crate::source::{self, AvailableVersion, GlobalCache, ResolvedRef};
use crate::sync::apply::ApplyResult;
pub use crate::sync::apply::SyncOptions;
use crate::types::{CommitHash, ItemName, SourceName};
use crate::validate::ValidationWarning;

/// Report from a completed sync operation.
#[derive(Debug)]
pub struct SyncReport {
    pub applied: ApplyResult,
    pub pruned: Vec<apply::ActionOutcome>,
    pub warnings: Vec<ValidationWarning>,
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
    /// Add or update a source in mars.toml.
    UpsertSource {
        name: SourceName,
        entry: SourceEntry,
    },
    /// Remove a source from mars.toml.
    RemoveSource { name: SourceName },
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
    /// Add a link target to settings.links (idempotent).
    SetLink { target: String },
    /// Remove a link target from settings.links.
    ClearLink { target: String },
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
pub fn mutate_link_config(root: &Path, mutation: &LinkMutation) -> Result<(), MarsError> {
    let lock_path = root.join(".mars").join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    let mut config = crate::config::load(root)?;
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
    crate::config::save(root, &config)?;

    Ok(())
}

/// Execute the unified sync pipeline.
pub fn execute(root: &Path, request: &SyncRequest) -> Result<SyncReport, MarsError> {
    validate_request(request)?;

    std::fs::create_dir_all(root.join(".mars").join("cache"))?;

    // Step 1: Acquire sync lock before any config reads/mutations.
    let lock_path = root.join(".mars").join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    // Step 2: Load config under lock (auto-init when mutating and missing).
    let mut config = match crate::config::load(root) {
        Ok(config) => config,
        Err(err) if is_config_not_found(&err) && request.mutation.is_some() => Config {
            sources: IndexMap::new(),
            settings: Settings::default(),
        },
        Err(err) => return Err(err),
    };

    // Step 3: Apply config mutation.
    let has_mutation = request.mutation.is_some();
    if let Some(mutation) = &request.mutation {
        apply_mutation(&mut config, mutation)?;
    }

    // Step 4: Load/mutate local overrides under the same lock.
    let mut local = crate::config::load_local(root)?;
    if let Some(mutation) = &request.mutation {
        apply_local_mutation(&mut local, mutation);
    }

    // Step 4b: Build effective config.
    let effective = crate::config::merge_with_root(config.clone(), local.clone(), root)?;

    // Step 5: Validate upgrade targets exist.
    validate_targets(&request.resolution, &effective)?;

    // Step 6: Load existing lock file.
    let old_lock = crate::lock::load(root)?;

    // Step 7: Resolve dependency graph.
    let cache = GlobalCache::new()?;
    let project_root = root.parent().unwrap_or(root);
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
        target::rewrite_skill_refs(&mut target_state, &renames, &graph)?;
    }

    // Step 10: Validate skill references.
    let warnings = validate_skill_refs(root, &target_state);

    // Step 11: Prevent managed installs from overwriting unmanaged files.
    target::check_unmanaged_collisions(root, &old_lock, &target_state)?;

    // Step 12: Compute diff.
    let sync_diff = diff::compute(root, &old_lock, &target_state, request.options.force)?;

    // Step 13: Create plan.
    let cache_bases_dir = root.join(".mars").join("cache").join("bases");
    let sync_plan = plan::create(&sync_diff, &request.options, &cache_bases_dir);

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
        match request.mutation {
            Some(ConfigMutation::SetOverride { .. } | ConfigMutation::ClearOverride { .. }) => {
                crate::config::save_local(root, &local)?;
            }
            Some(
                ConfigMutation::UpsertSource { .. }
                | ConfigMutation::RemoveSource { .. }
                | ConfigMutation::SetRename { .. }
                | ConfigMutation::SetLink { .. }
                | ConfigMutation::ClearLink { .. },
            ) => {
                crate::config::save(root, &config)?;
            }
            None => {}
        }
    }

    // Step 16: Apply plan.
    let applied = apply::execute(root, &sync_plan, &request.options, &cache_bases_dir)?;
    let pruned = Vec::new();

    // Step 17: Write lock file.
    if !request.options.dry_run {
        let new_lock = crate::lock::build(&graph, &applied, &old_lock)?;
        crate::lock::write(root, &new_lock)?;
    }

    Ok(SyncReport {
        applied,
        pruned,
        warnings,
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

fn apply_mutation(config: &mut Config, mutation: &ConfigMutation) -> Result<(), MarsError> {
    match mutation {
        ConfigMutation::UpsertSource { name, entry } => {
            if let Some(existing) = config.sources.get_mut(name) {
                // Merge: update source location fields, preserve user customizations
                existing.url = entry.url.clone();
                existing.path = entry.path.clone();
                existing.version = entry.version.clone();
                // Only overwrite filters if the new entry explicitly sets them
                if entry.filter.agents.is_some() {
                    existing.filter.agents = entry.filter.agents.clone();
                }
                if entry.filter.skills.is_some() {
                    existing.filter.skills = entry.filter.skills.clone();
                }
                if entry.filter.exclude.is_some() {
                    existing.filter.exclude = entry.filter.exclude.clone();
                }
                // Never overwrite rename rules from add — those are set via `mars rename`
                // entry.filter.rename is always None from the add command
            } else {
                config.sources.insert(name.clone(), entry.clone());
            }
            Ok(())
        }
        ConfigMutation::RemoveSource { name } => {
            if !config.sources.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.to_string(),
                    message: format!("source `{name}` not found in mars.toml"),
                });
            }
            config.sources.shift_remove(name);
            Ok(())
        }
        ConfigMutation::SetOverride { source_name, .. } => {
            if !config.sources.contains_key(source_name) {
                return Err(MarsError::Source {
                    source_name: source_name.to_string(),
                    message: format!("source `{source_name}` not found in mars.toml"),
                });
            }
            Ok(())
        }
        ConfigMutation::SetRename {
            source_name,
            from,
            to,
        } => {
            let source = config
                .sources
                .get_mut(source_name)
                .ok_or_else(|| MarsError::Source {
                    source_name: source_name.to_string(),
                    message: format!("source `{source_name}` not found in mars.toml"),
                })?;
            let rename_map = source
                .filter
                .rename
                .get_or_insert_with(crate::types::RenameMap::new);
            rename_map.insert(ItemName::from(from.as_str()), ItemName::from(to.as_str()));
            Ok(())
        }
        ConfigMutation::ClearOverride { .. } => Ok(()),
        ConfigMutation::SetLink { target } => {
            if !config.settings.links.contains(target) {
                config.settings.links.push(target.clone());
            }
            Ok(())
        }
        ConfigMutation::ClearLink { target } => {
            config.settings.links.retain(|l| l != target);
            Ok(())
        }
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
        ConfigMutation::UpsertSource { .. }
        | ConfigMutation::RemoveSource { .. }
        | ConfigMutation::SetRename { .. }
        | ConfigMutation::SetLink { .. }
        | ConfigMutation::ClearLink { .. } => {}
    }
}

fn validate_targets(
    resolution: &ResolutionMode,
    effective: &EffectiveConfig,
) -> Result<(), MarsError> {
    if let ResolutionMode::Maximize { targets } = resolution {
        for name in targets {
            if !effective.sources.contains_key(name) {
                return Err(MarsError::Source {
                    source_name: name.to_string(),
                    message: format!("source `{name}` not found in mars.toml"),
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
    fn read_manifest(
        &self,
        source_tree: &Path,
    ) -> Result<Option<crate::manifest::Manifest>, MarsError> {
        crate::manifest::load(source_tree)
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
        root: TempDir,
        source_trees: Vec<TempDir>,
    }

    impl TestFixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            // Create .mars/cache directories
            fs::create_dir_all(root.path().join(".mars/cache/bases")).unwrap();
            TestFixture {
                root,
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

        fn root(&self) -> &Path {
            self.root.path()
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
        let mut config_sources = IndexMap::new();

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

            config_sources.insert(
                name.into(),
                EffectiveSource {
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
                sources: config_sources,
                settings: Settings::default(),
            },
        )
    }

    fn path_source_entry(path: &Path) -> SourceEntry {
        SourceEntry {
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
            mutation: Some(ConfigMutation::RemoveSource {
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
        let root = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        fs::create_dir_all(source.path().join("agents")).unwrap();
        fs::write(source.path().join("agents/coder.md"), "# Coder").unwrap();

        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::UpsertSource {
                name: "base".into(),
                entry: path_source_entry(source.path()),
            }),
            options: SyncOptions::default(),
        };

        let report = execute(root.path(), &request).unwrap();
        assert!(!report.applied.outcomes.is_empty());
        assert!(root.path().join("mars.toml").exists());

        let saved = crate::config::load(root.path()).unwrap();
        assert!(saved.sources.contains_key("base"));
    }

    #[test]
    fn execute_dry_run_with_mutation_does_not_write_config() {
        let root = TempDir::new().unwrap();
        crate::config::save(
            root.path(),
            &Config {
                sources: IndexMap::new(),
                settings: Settings::default(),
            },
        )
        .unwrap();

        let source = TempDir::new().unwrap();
        fs::create_dir_all(source.path().join("agents")).unwrap();
        fs::write(source.path().join("agents/coder.md"), "# Coder").unwrap();

        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::UpsertSource {
                name: "base".into(),
                entry: path_source_entry(source.path()),
            }),
            options: SyncOptions {
                force: false,
                dry_run: true,
                frozen: false,
            },
        };

        let report = execute(root.path(), &request).unwrap();
        assert!(!report.applied.outcomes.is_empty());

        let saved = crate::config::load(root.path()).unwrap();
        assert!(!saved.sources.contains_key("base"));
        assert!(!root.path().join("agents/coder.md").exists());
        assert!(!root.path().join("mars.lock").exists());
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();

        // All items should be Add
        assert_eq!(sync_diff.items.len(), 2);
        for entry in &sync_diff.items {
            assert!(matches!(entry, diff::DiffEntry::Add { .. }));
        }

        // Create plan
        let cache_dir = fixture.root().join(".mars/cache/bases");
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
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        assert_eq!(result.outcomes.len(), 2);

        // Verify files were created
        assert!(fixture.root().join("agents/coder.md").exists());
        assert!(fixture.root().join("skills/planning/SKILL.md").exists());

        // Build lock
        let new_lock = crate::lock::build(&graph, &result, &lock).unwrap();
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Second sync with same content
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2, false).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Update source content
        let agents_dir = fixture.tree_path(src_idx).join("agents");
        fs::write(agents_dir.join("coder.md"), "# Version 2").unwrap();

        // Rebuild target with updated content
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2, false).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Locally modify the installed file
        fs::write(fixture.root().join("agents/coder.md"), "# Locally modified").unwrap();

        // Re-sync (source unchanged)
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2, false).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        // Locally modify the installed file
        fs::write(fixture.root().join("agents/coder.md"), "# Locally modified").unwrap();

        // Update source too (triggers conflict)
        let agents_dir = fixture.tree_path(src_idx).join("agents");
        fs::write(agents_dir.join("coder.md"), "# Upstream update").unwrap();

        // Re-sync with --force
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2, false).unwrap();

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

        let result2 =
            apply::execute(fixture.root(), &sync_plan2, &force_options, &cache_dir).unwrap();
        assert!(matches!(
            result2.outcomes[0].action,
            apply::ActionTaken::Updated
        ));

        // File should have upstream content
        let content = fs::read_to_string(fixture.root().join("agents/coder.md")).unwrap();
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();
        let first_lock = crate::lock::build(&graph, &result, &lock).unwrap();

        assert!(fixture.root().join("agents/coder.md").exists());
        assert!(fixture.root().join("agents/reviewer.md").exists());

        // Remove reviewer from source
        fs::remove_file(fixture.tree_path(src_idx).join("agents/reviewer.md")).unwrap();

        // Re-sync
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2, false).unwrap();

        // Should have one Unchanged and one Orphan
        let orphan_count = sync_diff2
            .items
            .iter()
            .filter(|e| matches!(e, diff::DiffEntry::Orphan { .. }))
            .count();
        assert_eq!(orphan_count, 1);

        let sync_plan2 = plan::create(&sync_diff2, &options, &cache_dir);
        let result2 = apply::execute(fixture.root(), &sync_plan2, &options, &cache_dir).unwrap();

        // Reviewer should be removed
        assert!(!fixture.root().join("agents/reviewer.md").exists());
        // Coder should still be there
        assert!(fixture.root().join("agents/coder.md").exists());

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();

        let cache_dir = fixture.root().join(".mars/cache/bases");
        let dry_options = SyncOptions {
            force: false,
            dry_run: true,
            frozen: false,
        };

        let sync_plan = plan::create(&sync_diff, &dry_options, &cache_dir);
        assert!(!sync_plan.actions.is_empty());

        // Execute in dry-run mode
        let result = apply::execute(fixture.root(), &sync_plan, &dry_options, &cache_dir).unwrap();
        assert!(!result.outcomes.is_empty());

        // No files should have been created
        assert!(!fixture.root().join("agents/coder.md").exists());
    }

    #[test]
    fn lock_written_after_apply() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Coder")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // Full pipeline minus actual sync() (which needs real config files)
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();

        let new_lock = crate::lock::build(&graph, &result, &lock).unwrap();
        crate::lock::write(fixture.root(), &new_lock).unwrap();

        // Verify lock file exists and is valid
        let reloaded = crate::lock::load(fixture.root()).unwrap();
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target, false).unwrap();
        let cache_dir = fixture.root().join(".mars/cache/bases");
        let options = SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
        };
        let sync_plan = plan::create(&sync_diff, &options, &cache_dir);
        let result = apply::execute(fixture.root(), &sync_plan, &options, &cache_dir).unwrap();

        assert!(fixture.root().join("agents/coder.md").exists());
        assert!(fixture.root().join("agents/reviewer.md").exists());
        assert_eq!(result.outcomes.len(), 2);
    }
}
