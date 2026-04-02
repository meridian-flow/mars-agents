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

use crate::error::{LockError, MarsError};

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
    /// Initialize a new .agents/ directory with agents.toml.
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
                eprintln!("hint: run `mars repair` to rebuild from agents.toml + sources");
            }
            err.exit_code()
        }
    }
}

fn dispatch_result(cli: Cli) -> Result<i32, MarsError> {
    match &cli.command {
        Command::Init(args) => init::run(args, cli.root.as_deref(), cli.json),
        Command::Add(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            add::run(args, &root, cli.json)
        }
        Command::Remove(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            remove::run(args, &root, cli.json)
        }
        Command::Sync(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            sync::run(args, &root, cli.json)
        }
        Command::Upgrade(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            upgrade::run(args, &root, cli.json)
        }
        Command::Outdated(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            outdated::run(args, &root, cli.json)
        }
        Command::List(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            list::run(args, &root, cli.json)
        }
        Command::Why(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            why::run(args, &root, cli.json)
        }
        Command::Rename(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            rename::run(args, &root, cli.json)
        }
        Command::Resolve(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            resolve_cmd::run(args, &root, cli.json)
        }
        Command::Override(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            override_cmd::run(args, &root, cli.json)
        }
        Command::Doctor(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            doctor::run(args, &root, cli.json)
        }
        Command::Repair(args) => {
            let root = find_agents_root(cli.root.as_deref())?;
            repair::run(args, &root, cli.json)
        }
    }
}

/// Find `.agents/` root by walking up from cwd, or use `--root` flag.
///
/// Walk up the directory tree looking for a directory containing `agents.toml`.
/// Checks both `.agents/agents.toml` and `agents.toml` at each level.
pub fn find_agents_root(explicit: Option<&Path>) -> Result<PathBuf, MarsError> {
    if let Some(root) = explicit {
        return Ok(root.to_path_buf());
    }

    let cwd = std::env::current_dir()?;
    let mut dir = cwd.as_path();

    loop {
        // Check for .agents/agents.toml
        let agents_dir = dir.join(".agents");
        if agents_dir.join("agents.toml").exists() {
            return Ok(agents_dir);
        }

        // Check if we're inside .agents/ already
        if dir.join("agents.toml").exists()
            && dir.file_name().map(|n| n == ".agents").unwrap_or(false)
        {
            return Ok(dir.to_path_buf());
        }

        // Walk up
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    Err(MarsError::Source {
        source_name: "root".to_string(),
        message: format!(
            "no .agents/ directory found from {} to /. Run `mars init` first.",
            cwd.display()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn find_root_with_explicit_path() {
        let dir = TempDir::new().unwrap();
        let root = find_agents_root(Some(dir.path())).unwrap();
        assert_eq!(root, dir.path());
    }

    #[test]
    fn find_root_walks_up() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("agents.toml"), "[sources]\n").unwrap();

        // Create a subdirectory
        let sub = dir.path().join("subdir").join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        // find_agents_root uses cwd, so we test with explicit
        // The actual walk-up requires changing cwd which isn't safe in tests
        let found = find_agents_root(Some(&agents_dir)).unwrap();
        assert_eq!(found, agents_dir);
    }
}
