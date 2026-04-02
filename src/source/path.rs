//! Path source adapter — resolves local filesystem paths.
//!
//! Path sources are always "live" — no caching, no copying.
//! Returns the resolved path directly.

use std::path::{Path, PathBuf};

use crate::error::MarsError;
use crate::source::ResolvedRef;
use crate::types::SourceName;

/// Fetch a path source: resolve relative paths against project root, verify exists.
///
/// - Relative paths are resolved against `project_root`.
/// - Absolute paths are used as-is.
/// - The path must exist and be a directory.
/// - Returns `ResolvedRef` with no version/commit (path sources are unversioned).
pub fn fetch_path(
    path: &Path,
    project_root: &Path,
    source_name: &str,
) -> Result<ResolvedRef, MarsError> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };

    // Canonicalize to resolve symlinks and `..` components
    let resolved = canonicalize_path(&resolved, source_name)?;

    // Verify the path exists and is a directory
    if !resolved.exists() {
        return Err(MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("path does not exist: {}", resolved.display()),
        });
    }

    if !resolved.is_dir() {
        return Err(MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("path is not a directory: {}", resolved.display()),
        });
    }

    Ok(ResolvedRef {
        source_name: SourceName::from(source_name),
        version: None,
        version_tag: None,
        commit: None,
        tree_path: resolved,
    })
}

/// Canonicalize a path, providing a helpful error on failure.
fn canonicalize_path(path: &Path, source_name: &str) -> Result<PathBuf, MarsError> {
    path.canonicalize().map_err(|e| MarsError::Source {
        source_name: source_name.to_string(),
        message: format!("failed to resolve path `{}`: {e}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fetch_absolute_path() {
        let dir = TempDir::new().unwrap();
        let source_dir = dir.path().join("my-agents");
        std::fs::create_dir_all(&source_dir).unwrap();

        let resolved = fetch_path(&source_dir, dir.path(), "local-source").unwrap();

        assert_eq!(resolved.source_name, "local-source");
        assert!(resolved.version.is_none());
        assert!(resolved.version_tag.is_none());
        assert!(resolved.commit.is_none());
        assert_eq!(
            resolved.tree_path.canonicalize().unwrap(),
            source_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn fetch_relative_path() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("project");
        let source_dir = dir.path().join("project").join("local-agents");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&source_dir).unwrap();

        let resolved = fetch_path(Path::new("local-agents"), &project_root, "local").unwrap();

        assert_eq!(
            resolved.tree_path.canonicalize().unwrap(),
            source_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn fetch_relative_path_with_dotdot() {
        let dir = TempDir::new().unwrap();
        let project_root = dir.path().join("project");
        let source_dir = dir.path().join("external-agents");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&source_dir).unwrap();

        let resolved =
            fetch_path(Path::new("../external-agents"), &project_root, "external").unwrap();

        assert_eq!(
            resolved.tree_path.canonicalize().unwrap(),
            source_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn fetch_nonexistent_path_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = fetch_path(&dir.path().join("nonexistent"), dir.path(), "bad-source");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bad-source"),
            "error should mention source name: {err}"
        );
        assert!(
            err.contains("nonexistent"),
            "error should mention the path: {err}"
        );
    }

    #[test]
    fn fetch_file_not_directory_returns_error() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("not-a-dir.txt");
        std::fs::write(&file_path, "content").unwrap();

        let result = fetch_path(&file_path, dir.path(), "file-source");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not a directory"),
            "error should mention 'not a directory': {err}"
        );
    }

}
