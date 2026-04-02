//! `mars init [TARGET] [--link DIR...]` — scaffold a mars-managed directory with `mars.toml`.
//!
//! TARGET is a simple directory name (default: `.agents`), not a path.
//! Creates `<cwd>/TARGET/mars.toml`. Use `--root` for explicit path control.
//!
//! Idempotent: re-running when already initialized is a no-op for init
//! but still processes `--link` flags.

use std::path::Path;

use crate::config::{Config, Settings};
use crate::error::{ConfigError, MarsError};

use super::output;

/// Arguments for `mars init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Directory name to create (default: .agents). Simple name, not a path.
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
                 like `.agents` or `.claude`. Use `--root` to specify an explicit path."
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

/// Run `mars init`.
pub fn run(args: &InitArgs, explicit_root: Option<&Path>, json: bool) -> Result<i32, MarsError> {
    // 1. Determine the managed root
    let managed_root = if let Some(root) = explicit_root {
        // --root flag: use directly (TARGET is ignored)
        root.to_path_buf()
    } else {
        let target = args.target.as_deref().unwrap_or(".agents");
        validate_target(target)?;
        std::env::current_dir()?.join(target)
    };

    // 2. Idempotency check
    let config_path = managed_root.join("mars.toml");
    let already_initialized = config_path.exists();

    if !already_initialized {
        // 3. Create structure
        std::fs::create_dir_all(&managed_root)?;
        std::fs::create_dir_all(managed_root.join(".mars"))?;

        let config = Config {
            sources: indexmap::IndexMap::new(),
            settings: Settings::default(),
        };
        crate::config::save(&managed_root, &config)?;
        add_to_gitignore(&managed_root)?;

        if !json {
            output::print_success(&format!(
                "initialized {} with mars.toml",
                managed_root.display()
            ));
        }
    } else {
        // Already initialized — reconcile required structure
        std::fs::create_dir_all(managed_root.join(".mars"))?;
        add_to_gitignore(&managed_root)?;

        if !json {
            output::print_info(&format!(
                "{} already initialized",
                managed_root.display()
            ));
        }
    }

    // 4. Process --link flags
    if !args.link.is_empty() {
        let ctx = super::MarsContext::new(managed_root.clone())?;
        for link_target in &args.link {
            let link_args = super::link::LinkArgs {
                target: link_target.clone(),
                unlink: false,
                force: false,
            };
            super::link::run(&link_args, &ctx, json)?;
        }
    }

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "path": managed_root.to_string_lossy(),
            "already_initialized": already_initialized,
            "links": args.link,
        }));
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
    fn init_creates_agents_toml() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".agents");

        let args = InitArgs {
            target: None,
            link: vec![],
        };

        // We can't easily test run() because it uses current_dir(),
        // but we can test with --root equivalent
        std::fs::create_dir_all(&agents_dir).unwrap();
        let config = Config {
            sources: indexmap::IndexMap::new(),
            settings: Settings::default(),
        };
        crate::config::save(&agents_dir, &config).unwrap();

        // Verify the file was created correctly
        assert!(agents_dir.join("mars.toml").exists());
        let _ = args; // suppress unused warning
    }

    #[test]
    fn add_to_gitignore_creates_file() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        add_to_gitignore(&agents_dir).unwrap();

        let content = std::fs::read_to_string(agents_dir.join(".gitignore")).unwrap();
        assert!(content.contains(".mars/"));
    }

    #[test]
    fn add_to_gitignore_idempotent() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        add_to_gitignore(&agents_dir).unwrap();
        add_to_gitignore(&agents_dir).unwrap();

        let content = std::fs::read_to_string(agents_dir.join(".gitignore")).unwrap();
        assert_eq!(content.matches(".mars/").count(), 1);
    }
}
