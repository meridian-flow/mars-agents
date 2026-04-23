//! Atomic filesystem operations for durable writes and directory replacement.
//!
//! All durable Mars writes should go through this module.

use std::fs;
use std::path::Path;

use crate::error::MarsError;

pub use crate::fs::{
    FLAT_SKILL_EXCLUDED_TOP_LEVEL, atomic_install_dir, atomic_install_dir_filtered, atomic_write,
    remove_item,
};

#[cfg(windows)]
pub use crate::fs::clear_readonly;

/// Result of cache directory publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePublishResult {
    /// The directory was published (renamed from temp to destination).
    Published,
    /// The destination already existed; temp was removed.
    AlreadyPresent,
}

/// Replace a generated directory with rollback semantics.
pub fn replace_generated_dir(src: &Path, dest: &Path) -> Result<(), MarsError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| io_context("create generated parent", parent, e))?;

    let old_path = parent.join(format!(
        ".{}.old",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));

    // Clean stale rollback content from prior crashes.
    if old_path.symlink_metadata().is_ok() {
        safe_remove(&old_path)?;
    }

    if dest.exists() {
        #[cfg(windows)]
        clear_readonly_recursive(dest)?;

        fs::rename(dest, &old_path)
            .map_err(|e| io_context("rename destination to backup", dest, e))?;

        if let Err(e) = fs::rename(src, dest) {
            let _ = fs::rename(&old_path, dest);
            let _ = safe_remove(src);
            return Err(io_context("rename source to destination", src, e));
        }

        let _ = safe_remove(&old_path);
    } else {
        fs::rename(src, dest).map_err(|e| io_context("rename source to destination", src, e))?;
    }

    Ok(())
}

/// Publish a cache directory iff destination is absent.
pub fn publish_cache_dir_if_absent(
    src: &Path,
    dest: &Path,
) -> Result<CachePublishResult, MarsError> {
    if dest.exists() {
        safe_remove(src)?;
        return Ok(CachePublishResult::AlreadyPresent);
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| io_context("create cache parent", parent, e))?;
    }

    match fs::rename(src, dest) {
        Ok(()) => Ok(CachePublishResult::Published),
        Err(_err) if dest.exists() => {
            let _ = safe_remove(src);
            Ok(CachePublishResult::AlreadyPresent)
        }
        Err(e) => Err(io_context("publish cache directory", src, e)),
    }
}

/// Remove a file or directory tree safely.
pub fn safe_remove(path: &Path) -> Result<(), MarsError> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_context("read metadata for removal", path, e)),
    };

    #[cfg(windows)]
    if metadata.is_dir() {
        clear_readonly_recursive(path)?;
    } else {
        clear_readonly(path).map_err(|e| io_context("clear readonly bit", path, e))?;
    }

    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|e| io_context("remove directory", path, e))?;
    } else {
        fs::remove_file(path).map_err(|e| io_context("remove file", path, e))?;
    }

    Ok(())
}

#[cfg(windows)]
fn clear_readonly_recursive(path: &Path) -> Result<(), MarsError> {
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        clear_readonly(entry.path())
            .map_err(|e| io_context("clear readonly bit", entry.path(), e))?;
    }
    Ok(())
}

fn io_context(operation: &str, path: &Path, source: std::io::Error) -> MarsError {
    MarsError::Io {
        operation: operation.to_string(),
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn replace_generated_dir_basic() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");

        fs::create_dir(&src).unwrap();
        fs::write(src.join("file.txt"), "content").unwrap();

        replace_generated_dir(&src, &dest).unwrap();

        assert!(!src.exists());
        assert!(dest.join("file.txt").exists());
    }

    #[test]
    fn replace_generated_dir_replaces_existing() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");

        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old.txt"), "old").unwrap();

        fs::create_dir(&src).unwrap();
        fs::write(src.join("new.txt"), "new").unwrap();

        replace_generated_dir(&src, &dest).unwrap();

        assert!(!dest.join("old.txt").exists());
        assert!(dest.join("new.txt").exists());
    }

    #[test]
    fn publish_cache_dir_if_absent_publishes() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");

        fs::create_dir(&src).unwrap();
        fs::write(src.join("file.txt"), "content").unwrap();

        let result = publish_cache_dir_if_absent(&src, &dest).unwrap();

        assert_eq!(result, CachePublishResult::Published);
        assert!(!src.exists());
        assert!(dest.join("file.txt").exists());
    }

    #[test]
    fn publish_cache_dir_if_absent_accepts_existing() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");

        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("existing.txt"), "existing").unwrap();

        fs::create_dir(&src).unwrap();
        fs::write(src.join("new.txt"), "new").unwrap();

        let result = publish_cache_dir_if_absent(&src, &dest).unwrap();

        assert_eq!(result, CachePublishResult::AlreadyPresent);
        assert!(!src.exists());
        assert!(dest.join("existing.txt").exists());
        assert!(!dest.join("new.txt").exists());
    }

    #[test]
    fn safe_remove_handles_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent");

        safe_remove(&path).unwrap();
    }
}
