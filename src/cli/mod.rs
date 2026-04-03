//! CLI layer — clap definitions + command dispatch.
//!
//! Each subcommand is a separate module. The CLI layer:
//! - Parses args into typed commands
//! - Locates project root (walk up from cwd, or `--root` flag)
//! - Calls library functions
//! - Formats output (human-readable by default, `--json` for machine)
//! - Maps `MarsError` to exit codes and stderr messages

pub mod add;
pub mod cache;
pub mod check;
pub mod doctor;
pub mod init;
pub mod link;
pub mod list;
pub mod outdated;
pub mod output;
pub mod override_cmd;
pub mod remove;
pub mod rename;
pub mod repair;
pub mod resolve_cmd;
pub mod sync;
pub mod upgrade;
pub mod why;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::error::{ConfigError, LockError, MarsError};
pub use crate::types::MarsContext;

/// Directories where mars materializes agents/skills output.
/// `.agents/` remains the default target for `mars init`.
pub const WELL_KNOWN: &[&str] = &[".agents"];

/// Tool-specific directories that commonly need linking.
/// `mars link` warns if the target isn't in TOOL_DIRS or WELL_KNOWN.
pub const TOOL_DIRS: &[&str] = &[".claude", ".cursor"];

impl MarsContext {
    /// Build context from project root (directory containing mars.toml).
    pub fn new(project_root: PathBuf) -> Result<Self, MarsError> {
        let project_canon = if project_root.exists() {
            project_root.canonicalize().unwrap_or(project_root.clone())
        } else {
            project_root.clone()
        };

        let managed_root = detect_managed_root(&project_canon)?;
        Self::from_roots(project_canon, managed_root)
    }

    /// Build context from explicit project and managed roots.
    pub fn from_roots(project_root: PathBuf, managed_root: PathBuf) -> Result<Self, MarsError> {
        let project_canon = if project_root.exists() {
            project_root.canonicalize().unwrap_or(project_root.clone())
        } else {
            project_root.clone()
        };
        let managed_canon = if managed_root.exists() {
            managed_root.canonicalize().unwrap_or(managed_root.clone())
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
        })
    }
}

/// mars — agent package manager for .agents/
#[derive(Debug, Parser)]
#[command(name = "mars", version, about = "Agent package manager for .agents/")]
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
    /// Initialize project-level mars.toml (managed dir default: .agents/).
    Init(init::InitArgs),

    /// Add a source (git URL, GitHub shorthand, or local path).
    Add(add::AddArgs),

    /// Remove a source.
    Remove(remove::RemoveArgs),

    /// Sync: resolve + install (make reality match config).
    Sync(sync::SyncArgs),

    /// Upgrade sources to newest compatible versions.
    Upgrade(upgrade::UpgradeArgs),

    /// Show available updates without applying.
    Outdated(outdated::OutdatedArgs),

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

    /// Symlink agents/ and skills/ into another directory (e.g. .claude).
    Link(link::LinkArgs),

    /// Validate a source package before publishing (structure, frontmatter, deps).
    Check(check::CheckArgs),

    /// Diagnose problems in an installed mars project (config, lock, files, links).
    Doctor(doctor::DoctorArgs),

    /// Rebuild state from lock + sources.
    Repair(repair::RepairArgs),

    /// Manage the global source cache.
    Cache(cache::CacheArgs),
}

/// Dispatch a parsed CLI command to the appropriate handler and map errors to
/// the final exit code.
pub fn dispatch(cli: Cli) -> i32 {
    match dispatch_result(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            if matches!(err, MarsError::Lock(LockError::Corrupt { .. })) {
                eprintln!("hint: run `mars repair` to rebuild from mars.toml + sources");
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
            let ctx = find_agents_root(cli.root.as_deref())?;
            dispatch_with_root(cmd, &ctx, cli.json)
        }
    }
}

fn dispatch_with_root(cmd: &Command, ctx: &MarsContext, json: bool) -> Result<i32, MarsError> {
    match cmd {
        Command::Add(args) => add::run(args, ctx, json),
        Command::Remove(args) => remove::run(args, ctx, json),
        Command::Sync(args) => sync::run(args, ctx, json),
        Command::Upgrade(args) => upgrade::run(args, ctx, json),
        Command::Outdated(args) => outdated::run(args, ctx, json),
        Command::List(args) => list::run(args, ctx, json),
        Command::Why(args) => why::run(args, ctx, json),
        Command::Rename(args) => rename::run(args, ctx, json),
        Command::Resolve(args) => resolve_cmd::run(args, ctx, json),
        Command::Override(args) => override_cmd::run(args, ctx, json),
        Command::Link(args) => link::run(args, ctx, json),
        Command::Doctor(args) => doctor::run(args, ctx, json),
        Command::Repair(args) => repair::run(args, ctx, json),
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
    // 1. Check settings in mars.toml
    match crate::config::load(project_root) {
        Ok(config) => {
            if let Some(name) = &config.settings.managed_root {
                return Ok(project_root.join(name));
            }
        }
        // Config doesn't exist yet (before mars init) — expected, fall through
        Err(MarsError::Config(ConfigError::NotFound { .. })) => {}
        // Config exists but has parse errors — surface the real error
        Err(e) => return Err(e),
    }

    // 2. Default: .agents
    let default_root = project_root.join(WELL_KNOWN[0]);
    if default_root.exists() || is_symlink(&default_root) {
        return Ok(default_root);
    }

    // 3. Fallback: scan for .mars/ marker (legacy compat)
    let mut marked_roots: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(project_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join(".mars").exists() {
                marked_roots.push(path);
            }
        }
    }

    if marked_roots.len() == 1 {
        return Ok(marked_roots.remove(0));
    }

    for subdir in TOOL_DIRS {
        let candidate = project_root.join(subdir);
        if marked_roots.iter().any(|p| p == &candidate) {
            return Ok(candidate);
        }
    }

    marked_roots.sort();
    if let Some(first) = marked_roots.into_iter().next() {
        return Ok(first);
    }

    Ok(default_root)
}

/// Walk up from cwd to find the git root, defaulting to cwd if not in a git repo.
pub fn default_project_root() -> Result<PathBuf, MarsError> {
    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(cwd),
        }
    }
}

fn is_consumer_config(path: &Path) -> Result<bool, MarsError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(MarsError::Io(e)),
    };

    let value: toml::Value = toml::from_str(&content).map_err(|e| ConfigError::Invalid {
        message: format!("failed to parse {}: {e}", path.display()),
    })?;
    let Some(table) = value.as_table() else {
        return Ok(false);
    };

    Ok(table.contains_key("dependencies"))
}

/// Find mars project root by walking up from cwd (or using `--root`).
///
/// Walk-up checks `mars.toml` in each directory and stops at the first
/// git root (`.git`), never crossing into parent repositories.
pub fn find_agents_root(explicit: Option<&Path>) -> Result<MarsContext, MarsError> {
    if let Some(root) = explicit {
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

        let config_path = root.join("mars.toml");
        if !is_consumer_config(&config_path)? {
            return Err(MarsError::Config(ConfigError::Invalid {
                message: format!(
                    "{} does not contain a consumer mars.toml config. \
                     A file with only [package] is a package manifest; run `mars init` to add [dependencies].",
                    root.display()
                ),
            }));
        }
        return MarsContext::new(root.to_path_buf());
    }

    find_agents_root_from(None, &std::env::current_dir()?)
}

fn find_agents_root_from(_explicit: Option<&Path>, start: &Path) -> Result<MarsContext, MarsError> {
    let cwd_canon = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    let mut dir = cwd_canon.as_path();

    loop {
        let config_path = dir.join("mars.toml");
        if is_consumer_config(&config_path)? {
            return MarsContext::new(dir.to_path_buf());
        }

        // Never cross the current git root (or submodule root).
        if dir.join(".git").exists() {
            break;
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    Err(MarsError::Config(ConfigError::Invalid {
        message: format!(
            "no consumer mars.toml found from {} up to repository root. Run `mars init` first.",
            start.display()
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn find_root_with_explicit_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "[dependencies]\n").unwrap();

        let ctx = find_agents_root(Some(dir.path())).unwrap();
        assert_eq!(ctx.project_root, dir.path().canonicalize().unwrap());
        assert_eq!(ctx.managed_root, dir.path().join(".agents"));
    }

    #[test]
    fn package_manifest_without_dependencies_is_not_consumer() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = find_agents_root(Some(dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn find_root_with_default_managed_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "[dependencies]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();

        let ctx = MarsContext::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(ctx.project_root, dir.path().canonicalize().unwrap());
        assert_eq!(
            ctx.managed_root,
            dir.path().join(".agents").canonicalize().unwrap()
        );
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
            dir.path().join(".claude").canonicalize().unwrap()
        );
    }

    #[test]
    fn find_root_with_custom_managed_dir_marker() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("mars.toml"), "[dependencies]\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/.mars")).unwrap();

        let ctx = MarsContext::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            ctx.managed_root,
            dir.path().join(".claude").canonicalize().unwrap()
        );
    }

    #[test]
    fn context_rejects_symlinked_managed_root_outside_project() {
        let project_dir = TempDir::new().unwrap();
        let external_dir = TempDir::new().unwrap();
        std::fs::write(project_dir.path().join("mars.toml"), "[dependencies]\n").unwrap();

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
        assert_eq!(result, dir.path().join(".agents"));
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
}
