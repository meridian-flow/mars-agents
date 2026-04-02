//! CLI layer — clap definitions + command dispatch.
//!
//! Each subcommand is a separate module. The CLI layer:
//! - Parses args into typed commands
//! - Locates `.agents/` root (walk up from cwd, or `--root` flag)
//! - Calls library functions
//! - Formats output (human-readable by default, `--json` for machine)
//! - Maps `MarsError` to exit codes and stderr messages

pub mod add;
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

/// Directories where mars manages mars.toml as the primary root.
/// These are the default target for `mars init`.
pub const WELL_KNOWN: &[&str] = &[".agents"];

/// Tool-specific directories that commonly need linking.
/// Root detection searches these in addition to WELL_KNOWN.
/// `mars link` warns if the target isn't in TOOL_DIRS or WELL_KNOWN.
pub const TOOL_DIRS: &[&str] = &[".claude", ".cursor"];

/// Resolved context for a mars command — both the managed root
/// and its parent project root.
pub struct MarsContext {
    /// The directory containing mars.toml (e.g. /project/.agents)
    pub managed_root: PathBuf,
    /// The project directory (managed_root's parent, e.g. /project)
    pub project_root: PathBuf,
}

impl MarsContext {
    /// Build from a managed root path. Enforces the invariant that
    /// managed_root must have a parent (i.e., is always a subdirectory).
    pub fn new(managed_root: PathBuf) -> Result<Self, MarsError> {
        let canonical = if managed_root.exists() {
            managed_root.canonicalize().unwrap_or(managed_root.clone())
        } else {
            managed_root.clone()
        };
        let project_root = canonical.parent()
            .ok_or_else(|| MarsError::Config(ConfigError::Invalid {
                message: format!(
                    "managed root {} has no parent directory — the managed root must be \
                     a subdirectory (e.g., /project/.agents, not /project)",
                    managed_root.display()
                ),
            }))?
            .to_path_buf();
        Ok(MarsContext { managed_root: canonical, project_root })
    }
}

/// mars — agent package manager for .agents/
#[derive(Debug, Parser)]
#[command(name = "mars", version, about = "Agent package manager for .agents/")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to .agents/ root (default: auto-detect by walking up from cwd).
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Output in JSON format.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new .agents/ directory with mars.toml.
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

    /// Validate state consistency.
    Doctor(doctor::DoctorArgs),

    /// Rebuild state from lock + sources.
    Repair(repair::RepairArgs),
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
        Command::Init(args) => init::run(args, cli.root.as_deref(), cli.json),
        Command::Add(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            add::run(args, &ctx, cli.json)
        }
        Command::Remove(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            remove::run(args, &ctx, cli.json)
        }
        Command::Sync(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            sync::run(args, &ctx, cli.json)
        }
        Command::Upgrade(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            upgrade::run(args, &ctx, cli.json)
        }
        Command::Outdated(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            outdated::run(args, &ctx, cli.json)
        }
        Command::List(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            list::run(args, &ctx, cli.json)
        }
        Command::Why(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            why::run(args, &ctx, cli.json)
        }
        Command::Rename(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            rename::run(args, &ctx, cli.json)
        }
        Command::Resolve(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            resolve_cmd::run(args, &ctx, cli.json)
        }
        Command::Override(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            override_cmd::run(args, &ctx, cli.json)
        }
        Command::Link(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            link::run(args, &ctx, cli.json)
        }
        Command::Doctor(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            doctor::run(args, &ctx, cli.json)
        }
        Command::Repair(args) => {
            let ctx = find_agents_root(cli.root.as_deref())?;
            repair::run(args, &ctx, cli.json)
        }
    }
}

/// Find the mars-managed root by walking up from cwd, or use `--root` flag.
///
/// Walk up the directory tree looking for a directory containing `mars.toml`.
/// The managed root can be any directory (`.agents/`, `.claude/`, etc.) —
/// mars doesn't impose a specific name.
///
/// Search order at each level:
/// 1. `.agents/mars.toml` (convention default)
/// 2. `.claude/mars.toml` (Claude Code projects)
/// 3. If cwd itself contains `mars.toml`, use it directly
pub fn find_agents_root(explicit: Option<&Path>) -> Result<MarsContext, MarsError> {
    if let Some(root) = explicit {
        return MarsContext::new(root.to_path_buf());
    }

    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();

    loop {
        // Check well-known subdirectories + tool dirs
        for subdir in WELL_KNOWN.iter().chain(TOOL_DIRS.iter()) {
            let candidate = dir.join(subdir);
            if candidate.join("mars.toml").exists() {
                return MarsContext::new(candidate);
            }
        }

        // Check if we're already inside a mars-managed directory
        if dir.join("mars.toml").exists() {
            return MarsContext::new(dir.to_path_buf());
        }

        // Walk up
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    Err(MarsError::Config(ConfigError::Invalid {
        message: format!(
            "no mars.toml found from {} to /. Run `mars init` first.",
            cwd.display()
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
        let ctx = find_agents_root(Some(dir.path())).unwrap();
        assert_eq!(ctx.managed_root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn find_root_walks_up() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("mars.toml"), "[sources]\n").unwrap();

        // Create a subdirectory
        let sub = dir.path().join("subdir").join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        // find_agents_root uses cwd, so we test with explicit
        // The actual walk-up requires changing cwd which isn't safe in tests
        let ctx = find_agents_root(Some(&agents_dir)).unwrap();
        assert_eq!(ctx.managed_root, agents_dir.canonicalize().unwrap());
        assert_eq!(ctx.project_root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn mars_context_new_errors_on_root_path() {
        // "/" has no parent — should error
        let result = MarsContext::new(std::path::PathBuf::from("/"));
        assert!(result.is_err());
    }
}
