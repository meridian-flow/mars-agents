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
    if !config_path.exists() {
        crate::fs::atomic_write(&config_path, b"[dependencies]\n")?;
        return Ok(false);
    }

    let content = std::fs::read_to_string(&config_path)?;

    let value: toml::Value = toml::from_str(&content).map_err(|e| ConfigError::Invalid {
        message: format!("failed to parse {}: {e}", config_path.display()),
    })?;

    let table = value.as_table().ok_or_else(|| {
        MarsError::Config(ConfigError::Invalid {
            message: format!(
                "{} must contain a TOML table at the top level",
                config_path.display()
            ),
        })
    })?;

    if table.contains_key("dependencies") {
        return Ok(true); // already a consumer
    }

    if table.contains_key("package") {
        return Err(MarsError::Config(ConfigError::Invalid {
            message: "mars.toml contains [package] but no [dependencies]. To use this as both \
                a package and a consumer, add [dependencies] manually. Running `mars init` \
                won't modify an existing package manifest."
                .into(),
        }));
    }

    // File exists but has neither — treat as fresh
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
    std::fs::create_dir_all(managed_root.join(".mars"))?;

    let already_initialized = ensure_consumer_config(&project_root)?;

    // 4. Persist settings.managed_root when target != .agents
    if target != ".agents" {
        persist_managed_root(&project_root, &target)?;
    }

    add_to_gitignore(&managed_root)?;
    ensure_local_gitignored(&project_root)?;

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
                force: false,
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
            config.settings.managed_root = Some(target.to_string());
            crate::config::save(project_root, &config)?;
        }
        Err(MarsError::Config(ConfigError::NotFound { .. })) => {
            // Config will be created by ensure_consumer_config — skip
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Ensure mars.local.toml is in the project root .gitignore.
fn ensure_local_gitignored(project_root: &Path) -> Result<(), MarsError> {
    let gitignore_path = project_root.join(".gitignore");
    let entry = "mars.local.toml";

    if gitignore_path.exists() {
        let content = std::fs::read_to_string(&gitignore_path)?;
        if content.lines().any(|line| line.trim() == entry) {
            return Ok(());
        }
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

/// Add `.mars/` to `.gitignore` in the managed directory if not already present.
fn add_to_gitignore(managed_dir: &Path) -> Result<(), MarsError> {
    let gitignore_path = managed_dir.join(".gitignore");
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
    fn ensure_consumer_config_creates_root_mars_toml() {
        let dir = TempDir::new().unwrap();

        let already = ensure_consumer_config(dir.path()).unwrap();
        assert!(!already);

        let content = std::fs::read_to_string(dir.path().join("mars.toml")).unwrap();
        assert!(content.contains("[dependencies]"));
    }

    #[test]
    fn ensure_consumer_config_refuses_package_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let result = ensure_consumer_config(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("[package]") && err.contains("[dependencies]"),
            "should mention both sections: {err}"
        );
    }

    #[test]
    fn ensure_local_gitignored_creates_gitignore() {
        let dir = TempDir::new().unwrap();
        ensure_local_gitignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("mars.local.toml"));
    }

    #[test]
    fn ensure_local_gitignored_idempotent() {
        let dir = TempDir::new().unwrap();
        ensure_local_gitignored(dir.path()).unwrap();
        ensure_local_gitignored(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches("mars.local.toml").count(), 1);
    }

    #[test]
    fn add_to_gitignore_creates_file() {
        let dir = TempDir::new().unwrap();
        let managed_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&managed_dir).unwrap();

        add_to_gitignore(&managed_dir).unwrap();

        let content = std::fs::read_to_string(managed_dir.join(".gitignore")).unwrap();
        assert!(content.contains(".mars/"));
    }

    #[test]
    fn add_to_gitignore_idempotent() {
        let dir = TempDir::new().unwrap();
        let managed_dir = dir.path().join(".agents");
        std::fs::create_dir_all(&managed_dir).unwrap();

        add_to_gitignore(&managed_dir).unwrap();
        add_to_gitignore(&managed_dir).unwrap();

        let content = std::fs::read_to_string(managed_dir.join(".gitignore")).unwrap();
        assert_eq!(content.matches(".mars/").count(), 1);
    }
}
