//! `mars cache` — manage the global source cache.

use crate::error::MarsError;
use crate::source::GlobalCache;

use super::output;

/// Arguments for `mars cache`.
#[derive(Debug, clap::Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum CacheCommand {
    /// Remove all cached sources (archives + git clones).
    Clean(CacheCleanArgs),

    /// Show cache location and disk usage.
    Info(CacheInfoArgs),
}

/// Arguments for `mars cache clean`.
#[derive(Debug, clap::Args)]
pub struct CacheCleanArgs {}

/// Arguments for `mars cache info`.
#[derive(Debug, clap::Args)]
pub struct CacheInfoArgs {}

/// Run `mars cache <subcommand>`.
pub fn run(args: &CacheArgs, json: bool) -> Result<i32, MarsError> {
    match &args.command {
        CacheCommand::Clean(_) => run_clean(json),
        CacheCommand::Info(_) => run_info(json),
    }
}

fn run_clean(json: bool) -> Result<i32, MarsError> {
    let cache = GlobalCache::new()?;

    let archives = dir_size(&cache.archives_dir());
    let git = dir_size(&cache.git_dir());

    // Remove contents but keep the directory structure
    remove_dir_contents(&cache.archives_dir())?;
    remove_dir_contents(&cache.git_dir())?;

    let total = archives + git;

    if json {
        let payload = serde_json::json!({
            "freed_bytes": total,
            "archives_bytes": archives,
            "git_bytes": git,
        });
        println!("{}", payload);
    } else {
        output::print_info(&format!(
            "cleaned {} (archives: {}, git: {})",
            format_bytes(total),
            format_bytes(archives),
            format_bytes(git),
        ));
    }

    Ok(0)
}

fn run_info(json: bool) -> Result<i32, MarsError> {
    let cache = GlobalCache::new()?;

    let archives = dir_size(&cache.archives_dir());
    let git = dir_size(&cache.git_dir());
    let total = archives + git;
    let path = cache.root.display().to_string();

    if json {
        let payload = serde_json::json!({
            "path": path,
            "total_bytes": total,
            "archives_bytes": archives,
            "git_bytes": git,
        });
        println!("{}", payload);
    } else {
        println!("path:     {path}");
        println!("total:    {}", format_bytes(total));
        println!("archives: {}", format_bytes(archives));
        println!("git:      {}", format_bytes(git));
    }

    Ok(0)
}

/// Calculate total size of all files in a directory tree.
fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Remove all contents of a directory without removing the directory itself.
fn remove_dir_contents(path: &std::path::Path) -> Result<(), MarsError> {
    if !path.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            std::fs::remove_dir_all(&entry_path)?;
        } else {
            std::fs::remove_file(&entry_path)?;
        }
    }
    Ok(())
}

/// Format bytes as human-readable string.
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_ranges() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn dir_size_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(dir_size(dir.path()), 0);
    }

    #[test]
    fn dir_size_with_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), "world!").unwrap();
        assert_eq!(dir_size(dir.path()), 11); // 5 + 6
    }

    #[test]
    fn dir_size_nonexistent() {
        assert_eq!(dir_size(std::path::Path::new("/nonexistent/path")), 0);
    }

    #[test]
    fn remove_dir_contents_clears_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("b.txt"), "world").unwrap();

        remove_dir_contents(dir.path()).unwrap();

        assert!(dir.path().exists()); // directory itself survives
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0); // but empty
    }

    #[test]
    fn remove_dir_contents_nonexistent_ok() {
        assert!(remove_dir_contents(std::path::Path::new("/nonexistent")).is_ok());
    }
}
