//! `mars init [TARGET] [--link DIR...]` — scaffold a mars project.
//!
//! Creates `<project-root>/mars.toml` and `<project-root>/TARGET` (default: `.agents`).
//! Use `--root` to select an explicit project root.
//!
//! Idempotent: re-running is a no-op for initialization but still processes
//! `--link` flags.

use std::path::Path;

use crate::error::{ConfigError, MarsError};

use super::output;

/// Arguments for `mars init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Directory name to create for managed output (default: .agents).
    pub target: Option<String>,

    /// Directories to link after initialization. Repeatable.
    #[arg(long, value_name = "DIR")]
    pub link: Vec<String>,
}

/// Validate that a target is a simple directory name, not a path.
fn validate_target(target: &str) -> Result<(), MarsError> {
    if target.contains('/') || target.contains('\\') {
        return Err(MarsError::Config(ConfigError::Invalid {
            message: format!(
                "`{target}` looks like a path — TARGET should be a directory name \
                 like `.agents` or `.claude`. Use `--root` to specify project root."
            ),
        }));
    }
    if target == "." || target == ".." || target.is_empty() {
        return Err(MarsError::Config(ConfigError::Invalid {
            message: format!(
                "`{target}` is not a valid target name — use a directory name like `.agents` or `.claude`."
            ),
        }));
    }
    Ok(())
}

fn ensure_consumer_config(project_root: &Path) -> Result<bool, MarsError> {
    let config_path = project_root.join("mars.toml");
    if config_path.exists() {
        return Ok(true);
    }

    crate::fs::atomic_write(&config_path, b"[dependencies]\n")?;
    Ok(false)
}

/// Run `mars init`.
pub fn run(args: &InitArgs, explicit_root: Option<&Path>, json: bool) -> Result<i32, MarsError> {
    // 1. Determine project root
    let project_root = explicit_root.map(Path::to_path_buf).unwrap_or_else(|| {
        super::default_project_root().unwrap_or_else(|_| std::env::current_dir().unwrap())
    });

    // 2. Determine target: argument → existing settings.managed_root → .agents
    let target = if let Some(t) = args.target.as_deref() {
        t.to_string()
    } else {
        // Check existing config for persisted managed_root
        match crate::config::load(&project_root) {
            Ok(config) => config
                .settings
                .managed_root
                .unwrap_or_else(|| ".agents".into()),
            Err(_) => ".agents".into(),
        }
    };

    validate_target(&target)?;
    let managed_root = project_root.join(&target);

    // 3. Ensure project config + managed structure
    std::fs::create_dir_all(&managed_root)?;
    std::fs::create_dir_all(project_root.join(".mars"))?;

    let already_initialized = ensure_consumer_config(&project_root)?;

    // 4. Persist settings.managed_root.
    persist_managed_root(&project_root, &target)?;

    if !json {
        if already_initialized {
            output::print_info(&format!("{} already initialized", project_root.display()));
        } else {
            output::print_success(&format!(
                "initialized {} with mars.toml",
                project_root.display()
            ));
        }
    }

    // 5. Process --link flags
    if !args.link.is_empty() {
        let ctx = super::MarsContext::from_roots(project_root.clone(), managed_root.clone())?;
        for link_target in &args.link {
            let link_args = super::link::LinkArgs {
                target: link_target.clone(),
                unlink: false,
            };
            super::link::run(&link_args, &ctx, json)?;
        }
    }

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "project_root": project_root.to_string_lossy(),
            "managed_root": managed_root.to_string_lossy(),
            "already_initialized": already_initialized,
            "links": args.link,
        }));
    }

    Ok(0)
}

/// Persist managed_root in mars.toml [settings].
fn persist_managed_root(project_root: &Path, target: &str) -> Result<(), MarsError> {
    match crate::config::load(project_root) {
        Ok(mut config) => {
            config.settings.managed_root = if target == ".agents" {
                None
            } else {
                Some(target.to_string())
            };
            crate::config::save(project_root, &config)?;
        }
        Err(MarsError::Config(ConfigError::NotFound { .. })) => {
            // Config will be created by ensure_consumer_config — skip
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_target_accepts_simple_names() {
        assert!(validate_target(".agents").is_ok());
        assert!(validate_target(".claude").is_ok());
        assert!(validate_target("my-agents").is_ok());
    }

    #[test]
    fn validate_target_rejects_paths() {
        assert!(validate_target("./foo").is_err());
        assert!(validate_target("foo/bar").is_err());
        assert!(validate_target("/absolute/path").is_err());
    }

    #[test]
    fn validate_target_rejects_dots() {
        assert!(validate_target(".").is_err());
        assert!(validate_target("..").is_err());
    }

    #[test]
    fn validate_target_rejects_empty() {
        assert!(validate_target("").is_err());
    }

    #[test]
    fn ensure_consumer_config_creates_root_mars_toml() {
        let dir = TempDir::new().unwrap();

        let already = ensure_consumer_config(dir.path()).unwrap();
        assert!(!already);

        let content = std::fs::read_to_string(dir.path().join("mars.toml")).unwrap();
        assert!(content.contains("[dependencies]"));
    }

    #[test]
    fn ensure_consumer_config_accepts_existing_mars_toml() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let already = ensure_consumer_config(dir.path()).unwrap();
        assert!(already);
    }
}
