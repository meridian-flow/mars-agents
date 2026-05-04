//! CLI layer — clap definitions + command dispatch.
//!
//! Each subcommand is a separate module. The CLI layer:
//! - Parses args into typed commands
//! - Locates project root (walk up from cwd, or `--root` flag)
//! - Calls library functions
//! - Formats output (human-readable by default, `--json` for machine)
//! - Maps `MarsError` to exit codes and stderr messages

pub mod add;
pub mod adopt;
pub mod cache;
pub mod check;
pub mod doctor;
pub mod export;
pub mod init;
pub mod link;
pub mod list;
pub mod models;
pub mod outdated;
pub mod output;
pub mod override_cmd;
pub mod remove;
pub mod rename;
pub mod repair;
pub mod resolve_cmd;
pub mod sync;
pub mod unlink;
pub mod upgrade;
pub mod validate;
pub mod version;
pub mod why;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::error::{ConfigError, LockError, MarsError};
pub use crate::types::MarsContext;

/// Deprecated generic output directories still recognized for migration hints.
pub const WELL_KNOWN: &[&str] = &[".agents"];

/// Tool-specific directories that commonly need linking.
/// `mars link` warns if the target isn't in TOOL_DIRS or WELL_KNOWN.
pub const TOOL_DIRS: &[&str] = &[".claude", ".cursor"];

impl MarsContext {
    /// Build context from project root (directory containing mars.toml).
    pub fn new(project_root: PathBuf) -> Result<Self, MarsError> {
        let project_canon = if project_root.exists() {
            dunce::canonicalize(&project_root).unwrap_or(project_root.clone())
        } else {
            project_root.clone()
        };

        let managed_root = detect_managed_root(&project_canon)?;
        Self::from_roots(project_canon, managed_root)
    }

    /// Build context from explicit project and managed roots.
    pub fn from_roots(project_root: PathBuf, managed_root: PathBuf) -> Result<Self, MarsError> {
        let project_canon = if project_root.exists() {
            dunce::canonicalize(&project_root).unwrap_or(project_root.clone())
        } else {
            project_root.clone()
        };
        let managed_canon = if managed_root.exists() {
            dunce::canonicalize(&managed_root).unwrap_or(managed_root.clone())
        } else {
            managed_root.clone()
        };

        if !managed_canon.starts_with(&project_canon) {
            return Err(MarsError::Config(ConfigError::Invalid {
                message: format!(
                    "{} resolves to {} which is outside {}. \
                     The managed root may be a symlink. Use --root to override.",
                    managed_root.display(),
                    managed_canon.display(),
                    project_canon.display(),
                ),
            }));
        }

        Ok(MarsContext {
            managed_root: managed_canon,
            project_root: project_canon,
            meridian_managed: crate::types::meridian_managed_from_env(),
        })
    }
}

/// mars — agent package manager for agent and skill packages.
#[derive(Debug, Parser)]
#[command(name = "mars", version, about = "Agent package manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to project root containing mars.toml (default: auto-detect).
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Output in JSON format.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize project-level mars.toml and .mars/ compiled store.
    Init(init::InitArgs),

    /// Add a dependency (git URL, GitHub shorthand, or local path).
    Add(add::AddArgs),

    /// Adopt an unmanaged target item into `.mars-src/`, then sync.
    Adopt(adopt::AdoptArgs),

    /// Remove a dependency.
    Remove(remove::RemoveArgs),

    /// Sync: resolve + install (make reality match config).
    Sync(sync::SyncArgs),

    /// Upgrade dependencies to newest compatible versions.
    Upgrade(upgrade::UpgradeArgs),

    /// Show available updates without applying.
    Outdated(outdated::OutdatedArgs),

    /// Bump package version in mars.toml, commit, and tag.
    Version(version::VersionArgs),

    /// List managed items with status.
    List(list::ListArgs),

    /// Explain why an item is installed.
    Why(why::WhyArgs),

    /// Rename a managed item.
    Rename(rename::RenameArgs),

    /// Mark conflicts as resolved.
    Resolve(resolve_cmd::ResolveArgs),

    /// Set a local dev override for a source.
    Override(override_cmd::OverrideArgs),

    /// Add/remove managed target directories (e.g. .claude).
    Link(link::LinkArgs),

    /// Remove a managed target directory.
    Unlink(unlink::UnlinkArgs),

    /// Dry-run the compiler pipeline and report diagnostics without writing.
    Validate(validate::ValidateArgs),

    /// Export the compile plan as JSON (dry-run, no writes).
    Export(export::ExportArgs),

    /// Validate a source package before publishing (structure, frontmatter, deps).
    Check(check::CheckArgs),

    /// Diagnose problems in an installed mars project (config, lock, files, targets).
    Doctor(doctor::DoctorArgs),

    /// Rebuild state from lock + sources.
    Repair(repair::RepairArgs),

    /// Manage the global source cache.
    Cache(cache::CacheArgs),

    /// Manage model aliases and the models cache.
    Models(models::ModelsArgs),
}

/// Dispatch a parsed CLI command to the appropriate handler and map errors to
/// the final exit code.
pub fn dispatch(cli: Cli) -> i32 {
    match dispatch_result(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            if matches!(err, MarsError::Lock(LockError::Corrupt { .. })) {
                eprintln!("hint: run `mars repair` to rebuild from mars.toml + dependencies");
            }
            err.exit_code()
        }
    }
}

fn dispatch_result(cli: Cli) -> Result<i32, MarsError> {
    match &cli.command {
        // Root-free commands
        Command::Init(args) => init::run(args, cli.root.as_deref(), cli.json),
        Command::Check(args) => check::run(args, cli.json),
        Command::Cache(args) => cache::run(args, cli.json),
        // All other commands require context
        cmd => {
            let ctx = match find_agents_root(cli.root.as_deref()) {
                Ok(ctx) => ctx,
                Err(err) if should_auto_init_project(cmd, &err) => {
                    let initialized = init::initialize_project(cli.root.as_deref(), None)?;
                    if !cli.json {
                        output::print_info(&format!(
                            "auto-initialized {} with mars.toml",
                            initialized.project_root.display()
                        ));
                    }
                    MarsContext::from_roots(
                        initialized.project_root.clone(),
                        initialized
                            .managed_root
                            .clone()
                            .unwrap_or_else(|| initialized.project_root.join(".mars")),
                    )?
                }
                Err(err) => return Err(err),
            };
            dispatch_with_root(cmd, &ctx, cli.json)
        }
    }
}

fn should_auto_init_project(cmd: &Command, err: &MarsError) -> bool {
    matches!(cmd, Command::Add(_) | Command::Link(_))
        && matches!(
            err,
            MarsError::Config(ConfigError::ProjectRootNotFound { .. })
        )
}

fn dispatch_with_root(cmd: &Command, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    match cmd {
        Command::Validate(args) => validate::run(args, ctx, json),
        Command::Export(args) => export::run(args, ctx, json),
        Command::Add(args) => add::run(args, ctx, json),
        Command::Adopt(args) => adopt::run(args, ctx, json),
        Command::Remove(args) => remove::run(args, ctx, json),
        Command::Sync(args) => sync::run(args, ctx, json),
        Command::Upgrade(args) => upgrade::run(args, ctx, json),
        Command::Outdated(args) => outdated::run(args, ctx, json),
        Command::Version(args) => version::run(args, ctx, json),
        Command::List(args) => list::run(args, ctx, json),
        Command::Why(args) => why::run(args, ctx, json),
        Command::Rename(args) => rename::run(args, ctx, json),
        Command::Resolve(args) => resolve_cmd::run(args, ctx, json),
        Command::Override(args) => override_cmd::run(args, ctx, json),
        Command::Link(args) => link::run(args, ctx, json),
        Command::Unlink(args) => unlink::run(args, ctx, json),
        Command::Doctor(args) => doctor::run(args, ctx, json),
        Command::Repair(args) => repair::run(args, ctx, json),
        Command::Models(args) => models::run(args, ctx, json),
        // Root-free commands handled in dispatch_result — unreachable here
        Command::Init(_) | Command::Check(_) | Command::Cache(_) => unreachable!(),
    }
}

/// Check if a path is a symlink (uses symlink_metadata, doesn't follow).
pub fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn detect_managed_root(project_root: &Path) -> Result<PathBuf, MarsError> {
    // 1. Check explicit settings in mars.toml.
    match crate::config::load(project_root) {
        Ok(config) => {
            if let Some(name) = &config.settings.managed_root {
                return Ok(project_root.join(name));
            }
            if config
                .settings
                .targets
                .as_ref()
                .is_some_and(|targets| targets.iter().any(|target| target == WELL_KNOWN[0]))
            {
                return Ok(project_root.join(WELL_KNOWN[0]));
            }
        }
        // Config doesn't exist yet (before mars init) — expected, fall through
        Err(MarsError::Config(ConfigError::NotFound { .. })) => {}
        // Config exists but has parse errors — surface the real error
        Err(e) => return Err(e),
    }

    // 2. Canonical store default. Do not infer legacy `.agents/` ownership from
    // disk presence; doctor reports leftover target migration hints separately.
    Ok(project_root.join(".mars"))
}

/// Find mars project root by walking up from start path to filesystem root.
///
/// For context commands (`add`, `sync`, etc.), this walks from the start path
/// (cwd or `--root`) until it finds a `mars.toml`. Walk-up continues to filesystem
/// root — git boundaries do not stop the walk.
///
/// If `--root` is provided, it sets the walk-up start path, not a direct target.
pub fn find_agents_root(explicit: Option<&Path>) -> Result<MarsContext, MarsError> {
    let start = if let Some(root) = explicit {
        // Reject --root values that look like managed output directories
        if let Some(basename) = root.file_name().and_then(|f| f.to_str())
            && (WELL_KNOWN.contains(&basename) || TOOL_DIRS.contains(&basename))
        {
            return Err(MarsError::Config(ConfigError::Invalid {
                message: format!(
                    "`--root {basename}` looks like a managed output directory.\n  \
                     --root takes the project root (containing mars.toml), not the output directory.\n  \
                     Try: mars init  (auto-detects project root)\n  \
                     Or:  mars init {basename}  (specify output directory name)"
                ),
            }));
        }

        root.to_path_buf()
    } else {
        std::env::current_dir()?
    };

    find_agents_root_from(&start)
}

/// Walk up from `start` to filesystem root searching for `mars.toml`.
///
/// Uses `Path::parent()` which returns `None` at filesystem root on all platforms:
/// - Unix: `/` has no parent
/// - Windows: `C:\` or UNC roots like `\\server\share` have no parent
fn find_agents_root_from(start: &Path) -> Result<MarsContext, MarsError> {
    let start_canon = dunce::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut dir = start_canon.as_path();

    // Walk up to filesystem root (Path::parent() returns None at root)
    loop {
        let config_path = dir.join("mars.toml");
        if config_path.exists() {
            return MarsContext::new(dir.to_path_buf());
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    Err(MarsError::Config(ConfigError::ProjectRootNotFound {
        start: start.to_path_buf(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn find_root_with_explicit_path() {
        let dir = TempDir::new().unwrap();
        // Canonicalize once and use everywhere to avoid Windows 8.3 short-name mismatches
        let canonical_dir = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(canonical_dir.join("mars.toml"), "[dependencies]\n").unwrap();

        // --root points to a dir with mars.toml — should find it via walk-up
        let ctx = find_agents_root(Some(&canonical_dir)).unwrap();
        assert_eq!(ctx.project_root, canonical_dir);
        assert_eq!(ctx.managed_root, ctx.project_root.join(".mars"));
    }

    #[test]
    fn package_manifest_without_dependencies_is_valid_project_root() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let ctx = find_agents_root(Some(dir.path())).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn find_root_ignores_leftover_agents_dir_without_explicit_config() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "[dependencies]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();

        let ctx = MarsContext::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(dir.path()).unwrap());
        assert_eq!(ctx.managed_root, ctx.project_root.join(".mars"));
    }

    #[test]
    fn find_root_with_custom_managed_dir_from_settings() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[dependencies]\n\n[settings]\nmanaged_root = \".claude\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();

        let ctx = MarsContext::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ctx.managed_root,
            dunce::canonicalize(dir.path().join(".claude")).unwrap()
        );
    }

    #[test]
    fn find_root_with_agents_target_from_settings_targets() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[dependencies]\n\n[settings]\ntargets = [\".agents\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();

        let ctx = MarsContext::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ctx.managed_root,
            dunce::canonicalize(dir.path().join(".agents")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn context_rejects_symlinked_managed_root_outside_project() {
        let project_dir = TempDir::new().unwrap();
        let external_dir = TempDir::new().unwrap();
        std::fs::write(
            project_dir.path().join("mars.toml"),
            "[dependencies]\n\n[settings]\nmanaged_root = \".agents\"\n",
        )
        .unwrap();

        let external_agents = external_dir.path().join(".agents");
        std::fs::create_dir_all(&external_agents).unwrap();

        let project_agents = project_dir.path().join(".agents");
        std::os::unix::fs::symlink(&external_agents, &project_agents).unwrap();

        let result = MarsContext::new(project_dir.path().to_path_buf());
        assert!(result.is_err());
    }

    #[test]
    fn detect_managed_root_reads_settings() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[dependencies]\n\n[settings]\nmanaged_root = \".claude\"\n",
        )
        .unwrap();
        let result = detect_managed_root(dir.path()).unwrap();
        assert_eq!(result, dir.path().join(".claude"));
    }

    #[test]
    fn detect_managed_root_falls_through_on_missing_config() {
        let dir = TempDir::new().unwrap();
        let result = detect_managed_root(dir.path()).unwrap();
        assert_eq!(result, dir.path().join(".mars"));
    }

    #[test]
    fn detect_managed_root_ignores_agents_dir_without_explicit_config() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "[dependencies]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();

        let result = detect_managed_root(dir.path()).unwrap();
        assert_eq!(result, dir.path().join(".mars"));
    }

    #[test]
    fn detect_managed_root_surfaces_parse_errors() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "invalid toml {{{").unwrap();
        let result = detect_managed_root(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn init_rejects_root_that_looks_like_managed_dir() {
        let result = find_agents_root(Some(Path::new(".agents")));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("managed output directory"),
            "should reject .agents as --root: {err}"
        );
    }

    // ── Walk-up discovery tests (filesystem root boundary) ──────────────────────────

    #[test]
    fn walk_up_crosses_git_boundary_to_find_config() {
        // Outer has mars.toml, inner has .git but no mars.toml
        // Starting from inner SHOULD find outer's config (git is irrelevant)
        let dir = TempDir::new().unwrap();
        let outer = dir.path().join("outer");
        std::fs::create_dir_all(outer.join(".agents")).unwrap();
        std::fs::write(outer.join("mars.toml"), "[dependencies]\n").unwrap();

        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();

        let ctx = find_agents_root_from(&inner).unwrap();
        assert_eq!(
            ctx.project_root,
            dunce::canonicalize(&outer).unwrap(),
            "should find outer config even when inner has .git"
        );
    }

    #[test]
    fn walk_up_finds_config_in_ancestor() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join(".agents")).unwrap();
        std::fs::write(root.join("mars.toml"), "[dependencies]\n").unwrap();

        let subdir = root.join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        let ctx = find_agents_root_from(&subdir).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(&root).unwrap());
    }

    #[test]
    fn walk_up_prefers_nearest_mars_toml() {
        // child has package-only mars.toml, parent also has mars.toml
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir_all(parent.join(".agents")).unwrap();
        std::fs::write(parent.join("mars.toml"), "[dependencies]\n").unwrap();

        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            child.join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let ctx = find_agents_root_from(&child).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(&child).unwrap());
    }

    #[test]
    fn walk_up_from_deep_subdirectory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".agents")).unwrap();
        std::fs::write(root.join("mars.toml"), "[dependencies]\n").unwrap();

        let deep = root.join("src").join("foo").join("bar");
        std::fs::create_dir_all(&deep).unwrap();

        let ctx = find_agents_root_from(&deep).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(&root).unwrap());
    }

    #[test]
    fn walk_up_crosses_submodule_boundary() {
        // Outer repo has mars.toml
        // Inner dir has .git FILE (submodule marker) — walk-up should still find outer config
        let dir = TempDir::new().unwrap();
        let outer = dir.path().join("outer");
        std::fs::create_dir_all(outer.join(".agents")).unwrap();
        std::fs::write(outer.join("mars.toml"), "[dependencies]\n").unwrap();

        let submodule = outer.join("submodule");
        std::fs::create_dir_all(&submodule).unwrap();
        // .git FILE (not dir) marks a submodule
        std::fs::write(
            submodule.join(".git"),
            "gitdir: ../../.git/modules/submodule\n",
        )
        .unwrap();

        let ctx = find_agents_root_from(&submodule).unwrap();
        assert_eq!(
            ctx.project_root,
            dunce::canonicalize(&outer).unwrap(),
            "should find outer config through submodule .git file boundary"
        );
    }

    #[test]
    fn walk_up_errors_when_no_config_found() {
        let dir = TempDir::new().unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();

        let result = find_agents_root_from(&deep);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no mars.toml found"),
            "should report no config found: {err}"
        );
        assert!(
            err.contains("filesystem root"),
            "should mention filesystem root: {err}"
        );
    }

    #[test]
    fn walk_up_with_root_flag_starts_from_specified_path() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".agents")).unwrap();
        std::fs::write(project.join("mars.toml"), "[dependencies]\n").unwrap();

        // --root points to subdirectory — walk up should find mars.toml in parent
        let subdir = project.join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        let ctx = find_agents_root(Some(&subdir)).unwrap();
        assert_eq!(ctx.project_root, dunce::canonicalize(&project).unwrap());
    }
}
