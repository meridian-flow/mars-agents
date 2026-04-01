pub mod apply;
pub mod diff;
pub mod plan;
pub mod target;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::resolve::{ResolveOptions, SourceProvider};
use crate::source::{self, AvailableVersion, CacheDir, Fetchers, ResolvedRef};
use crate::sync::apply::{ApplyResult, SyncOptions};
use crate::validate::ValidationWarning;

/// Context for a sync operation.
///
/// Carries the root directory, fetchers, cache, and options.
/// Supports separating resolution root from install target for future
/// workspace support.
#[derive(Debug)]
pub struct SyncContext {
    /// `.agents/` directory — config + lock live here.
    pub root: PathBuf,
    /// Where items are installed (defaults to root).
    pub install_target: PathBuf,
    pub fetchers: Fetchers,
    pub cache: CacheDir,
    pub options: SyncOptions,
}

/// Report from a completed sync operation.
#[derive(Debug)]
pub struct SyncReport {
    pub applied: ApplyResult,
    pub pruned: Vec<apply::ActionOutcome>,
    pub warnings: Vec<ValidationWarning>,
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

/// Real source provider that delegates to the source module.
///
/// Implements the SourceProvider trait so the resolver can fetch sources
/// and read manifests through a uniform interface.
struct RealSourceProvider<'a> {
    cache_dir: &'a Path,
    project_root: &'a Path,
}

impl SourceProvider for RealSourceProvider<'_> {
    fn list_versions(&self, url: &str) -> Result<Vec<AvailableVersion>, MarsError> {
        source::list_versions(url, self.cache_dir)
    }

    fn fetch_git_version(
        &self,
        url: &str,
        version: &AvailableVersion,
        source_name: &str,
    ) -> Result<ResolvedRef, MarsError> {
        source::git::fetch(
            url,
            Some(&version.tag),
            source_name,
            self.cache_dir,
        )
    }

    fn fetch_git_ref(
        &self,
        url: &str,
        ref_name: &str,
        source_name: &str,
    ) -> Result<ResolvedRef, MarsError> {
        source::git::fetch(url, Some(ref_name), source_name, self.cache_dir)
    }

    fn fetch_path(
        &self,
        path: &Path,
        source_name: &str,
    ) -> Result<ResolvedRef, MarsError> {
        source::path::fetch_path(path, self.project_root, source_name)
    }

    fn read_manifest(
        &self,
        source_tree: &Path,
    ) -> Result<Option<crate::manifest::Manifest>, MarsError> {
        crate::manifest::load(source_tree)
    }
}

/// The complete sync pipeline — 15 steps matching the feature spec.
///
/// 1. Acquire sync lock
/// 2. Read agents.toml (merged with agents.local.toml)
/// 3. Load existing lock file
/// 4. Fetch sources + resolve dependency graph — abort on any fetch failure
/// 5. Discover items in each source, apply filtering
/// 6. Build target state (intent-based filtering)
/// 7. Detect collisions + auto-rename
/// 8. Rewrite frontmatter refs for renames
/// 9. Validate skill references
/// 10. Diff current state against target
/// 11. Create action plan
/// 12. Apply changes (or dry-run)
/// 13. Write new agents.lock
/// 14. Release lock (implicit via drop)
/// 15. Report results
pub fn sync(ctx: &SyncContext) -> Result<SyncReport, MarsError> {
    // Step 1: Acquire sync lock
    let lock_path = ctx.root.join(".mars").join("sync.lock");
    let _sync_lock = crate::fs::FileLock::acquire(&lock_path)?;

    // Step 2: Read config
    let config = crate::config::load(&ctx.root)?;
    let local = crate::config::load_local(&ctx.root)?;
    let effective = crate::config::merge(config, local)?;

    // Step 3: Load existing lock file
    let old_lock = crate::lock::load(&ctx.root)?;

    // Step 4: Resolve dependency graph (fetches sources)
    // This is where fetch failures will abort the pipeline
    let provider = RealSourceProvider {
        cache_dir: &ctx.cache.path,
        project_root: &ctx.root,
    };
    let resolve_options = ResolveOptions::default();
    let graph = crate::resolve::resolve(&effective, &provider, Some(&old_lock), &resolve_options)?;

    // Step 5-6: Build target state (includes discovery and filtering)
    let (mut target_state, renames) = target::build_with_collisions(&graph, &effective)?;

    // Step 7-8: Handle collisions + rewrite frontmatter refs
    if !renames.is_empty() {
        target::rewrite_skill_refs(&mut target_state, &renames, &graph)?;
    }

    // Step 9: Validate skill references
    let warnings = validate_skill_refs(&ctx.install_target, &target_state);

    // Step 10: Compute diff
    let sync_diff = diff::compute(&ctx.install_target, &old_lock, &target_state)?;

    // Step 11: Create plan
    let cache_bases_dir = ctx.root.join(".mars").join("cache").join("bases");
    let sync_plan = plan::create(&sync_diff, &ctx.options, &cache_bases_dir);

    // Check frozen mode before applying
    if ctx.options.frozen {
        let has_changes = sync_plan.actions.iter().any(|a| {
            !matches!(
                a,
                plan::PlannedAction::Skip { .. } | plan::PlannedAction::KeepLocal { .. }
            )
        });
        if has_changes {
            return Err(MarsError::Source {
                source_name: "sync".to_string(),
                message: "lock file would change but --frozen is set".to_string(),
            });
        }
    }

    // Step 12: Apply plan
    let applied = apply::execute(
        &ctx.install_target,
        &sync_plan,
        &ctx.options,
        &cache_bases_dir,
    )?;

    // Orphan pruning is handled by Remove actions in the plan (via diff::Orphan)
    let pruned = Vec::new();

    // Step 13: Write lock file (only if not dry-run and changes were made)
    if !ctx.options.dry_run {
        let new_lock = crate::lock::build(&graph, &applied, &old_lock)?;
        crate::lock::write(&ctx.root, &new_lock)?;
    }

    // Step 14: Lock released on drop of _sync_lock

    // Step 15: Report
    Ok(SyncReport {
        applied,
        pruned,
        warnings,
    })
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
        .map(|item| item.id.name.clone())
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
            (item.id.name.clone(), path)
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
                name.to_string(),
                ResolvedNode {
                    source_name: name.to_string(),
                    resolved_ref: ResolvedRef {
                        source_name: name.to_string(),
                        version: None,
                        version_tag: None,
                        commit: None,
                        tree_path: tree_path.clone(),
                    },
                    manifest: None,
                    deps: vec![],
                },
            );
            order.push(name.to_string());

            config_sources.insert(
                name.to_string(),
                EffectiveSource {
                    name: name.to_string(),
                    spec: SourceSpec::Path(tree_path),
                    filter,
                    rename: IndexMap::new(),
                    is_overridden: false,
                    original_git: None,
                },
            );
        }

        (
            ResolvedGraph { nodes, order },
            EffectiveConfig {
                sources: config_sources,
                settings: Settings {},
            },
        )
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2).unwrap();

        // Should detect an Update
        assert_eq!(sync_diff2.items.len(), 1);
        assert!(matches!(&sync_diff2.items[0], diff::DiffEntry::Update { .. }));
    }

    #[test]
    fn local_modification_preserved() {
        let mut fixture = TestFixture::new();
        let src_idx = fixture.add_source(&[("coder.md", "# Original")], &[]);

        let (graph, config) = make_graph_config(&fixture, vec![("base", src_idx, FilterMode::All)]);

        // First sync
        let (target, _) = target::build_with_collisions(&graph, &config).unwrap();
        let lock = LockFile::empty();
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        fs::write(
            fixture.root().join("agents/coder.md"),
            "# Locally modified",
        )
        .unwrap();

        // Re-sync (source unchanged)
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        fs::write(
            fixture.root().join("agents/coder.md"),
            "# Locally modified",
        )
        .unwrap();

        // Update source too (triggers conflict)
        let agents_dir = fixture.tree_path(src_idx).join("agents");
        fs::write(agents_dir.join("coder.md"), "# Upstream update").unwrap();

        // Re-sync with --force
        let (target2, _) = target::build_with_collisions(&graph, &config).unwrap();
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        let sync_diff2 = diff::compute(fixture.root(), &first_lock, &target2).unwrap();

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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();

        let cache_dir = fixture.root().join(".mars/cache/bases");
        let dry_options = SyncOptions {
            force: false,
            dry_run: true,
            frozen: false,
        };

        let sync_plan = plan::create(&sync_diff, &dry_options, &cache_dir);
        assert!(!sync_plan.actions.is_empty());

        // Execute in dry-run mode
        let result =
            apply::execute(fixture.root(), &sync_plan, &dry_options, &cache_dir).unwrap();
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
        let sync_diff = diff::compute(fixture.root(), &lock, &target).unwrap();
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
