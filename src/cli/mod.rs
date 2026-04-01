use clap::{Parser, Subcommand};

use crate::error::MarsError;

/// mars — agent package manager for .agents/
#[derive(Debug, Parser)]
#[command(name = "mars", version, about = "Agent package manager for .agents/")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Path to .agents/ root (default: auto-detect by walking up from cwd).
    #[arg(long, global = true)]
    pub root: Option<std::path::PathBuf>,

    /// Output in JSON format.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a new .agents/ directory with agents.toml.
    Init,

    /// Add a source (git URL or local path).
    Add {
        /// Source specifier (e.g., github.com/user/repo@v1.0 or ./local-path).
        source: String,

        /// Only install specific agents from this source.
        #[arg(long)]
        agents: Vec<String>,

        /// Only install specific skills from this source.
        #[arg(long)]
        skills: Vec<String>,

        /// Exclude specific items from this source.
        #[arg(long)]
        exclude: Vec<String>,
    },

    /// Remove a source.
    Remove {
        /// Name of the source to remove.
        source: String,
    },

    /// Sync: resolve + install (make reality match config).
    Sync {
        /// Force overwrite on conflicts (skip merge).
        #[arg(long)]
        force: bool,

        /// Dry run — show what would change without applying.
        #[arg(long)]
        diff: bool,

        /// Error if lock file would change (CI mode).
        #[arg(long)]
        frozen: bool,
    },

    /// Update sources within version constraints.
    Update {
        /// Specific source to update (default: all).
        source: Option<String>,
    },

    /// List installed items grouped by source.
    List,

    /// Show available updates without applying.
    Outdated,

    /// Trace which source provides an item.
    Why {
        /// Item name to trace.
        item: String,
    },

    /// Rename a managed item.
    Rename {
        /// Current item path (e.g., agents/coder).
        from: String,
        /// New item path (e.g., agents/cool-coder).
        to: String,
    },

    /// Set a local dev override for a source.
    Override {
        /// Source name to override.
        source: String,
        /// Local path to use instead.
        #[arg(long)]
        path: std::path::PathBuf,
    },

    /// Validate state consistency.
    Doctor,

    /// Rebuild state from lock + sources.
    Repair,
}

/// Dispatch a parsed CLI command to the appropriate handler.
///
/// Returns an exit code:
/// - 0: success
/// - 1: sync completed with unresolved conflicts
/// - 2: resolution/validation error
/// - 3: I/O or git error
pub fn dispatch(cli: Cli) -> Result<i32, MarsError> {
    match cli.command {
        Command::Init => {
            todo!("mars init")
        }
        Command::Add { .. } => {
            todo!("mars add")
        }
        Command::Remove { .. } => {
            todo!("mars remove")
        }
        Command::Sync { .. } => {
            todo!("mars sync")
        }
        Command::Update { .. } => {
            todo!("mars update")
        }
        Command::List => {
            todo!("mars list")
        }
        Command::Outdated => {
            todo!("mars outdated")
        }
        Command::Why { .. } => {
            todo!("mars why")
        }
        Command::Rename { .. } => {
            todo!("mars rename")
        }
        Command::Override { .. } => {
            todo!("mars override")
        }
        Command::Doctor => {
            todo!("mars doctor")
        }
        Command::Repair => {
            todo!("mars repair")
        }
    }
}
