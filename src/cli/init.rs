//! `mars init` — scaffold `.agents/agents.toml`.

use std::path::{Path, PathBuf};

use crate::config::{Config, Settings};
use crate::error::MarsError;

use super::output;

/// Arguments for `mars init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Path to initialize (default: current directory).
    pub path: Option<PathBuf>,
}

/// Run `mars init`.
pub fn run(args: &InitArgs, json: bool) -> Result<i32, MarsError> {
    let base = match &args.path {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };

    let agents_dir = if base.join("agents.toml").exists() || base.ends_with(".agents") {
        // Already inside .agents/ or pointing at it
        base.clone()
    } else {
        base.join(".agents")
    };

    let config_path = agents_dir.join("agents.toml");
    if config_path.exists() {
        return Err(MarsError::Source {
            source_name: "init".to_string(),
            message: format!(
                "agents.toml already exists at {}. Use `mars sync` instead.",
                config_path.display()
            ),
        });
    }

    // Create directories
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::create_dir_all(agents_dir.join(".mars"))?;

    // Write empty config
    let config = Config {
        sources: indexmap::IndexMap::new(),
        settings: Settings {},
    };
    crate::config::save(&agents_dir, &config)?;

    // Add .mars/ to .gitignore if not already there
    add_to_gitignore(&agents_dir)?;

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "path": agents_dir.to_string_lossy(),
        }));
    } else {
        output::print_success(&format!(
            "initialized {} with agents.toml",
            agents_dir.display()
        ));
    }

    Ok(0)
}

/// Add `.mars/` to `.gitignore` in the agents directory if not already present.
fn add_to_gitignore(agents_dir: &Path) -> Result<(), MarsError> {
    let gitignore_path = agents_dir.join(".gitignore");
    let entry = ".mars/";

    if gitignore_path.exists() {
        let content = std::fs::read_to_string(&gitignore_path)?;
        if content.lines().any(|line| line.trim() == entry) {
            return Ok(());
        }
        // Append
        let mut new_content = content;
        if !new_content.ends_with('\n') && !new_content.is_empty() {
            new_content.push('\n');
        }
        new_content.push_str(entry);
        new_content.push('\n');
        crate::fs::atomic_write(&gitignore_path, new_content.as_bytes())?;
    } else {
        crate::fs::atomic_write(&gitignore_path, format!("{entry}\n").as_bytes())?;
    }

    Ok(())
}
