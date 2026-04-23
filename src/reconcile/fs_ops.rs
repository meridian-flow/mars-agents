use std::fs;
use std::path::Path;

use crate::error::MarsError;
use crate::platform::fs::{replace_generated_dir, safe_remove as platform_safe_remove};
use crate::types::{ContentHash, ItemKind};

/// Atomic file write via tmp+rename in the same directory.
pub fn atomic_write_file(dest: &Path, content: &[u8]) -> Result<(), MarsError> {
    crate::fs::atomic_write(dest, content)
}

/// Atomic directory install: copy tree to tmp dir in same parent, then rename.
pub fn atomic_install_dir(source: &Path, dest: &Path) -> Result<(), MarsError> {
    crate::fs::atomic_install_dir(source, dest)
}

/// Atomic file copy: read source (following symlinks), write to tmp, rename to dest.
pub fn atomic_copy_file(source: &Path, dest: &Path) -> Result<(), MarsError> {
    let content = fs::read(source)?;
    #[cfg(windows)]
    if dest.exists() {
        crate::fs::clear_readonly(dest)?;
    }
    atomic_write_file(dest, &content)
}

/// Atomic directory copy: deep copy source tree (following symlinks) to tmp, rename to dest.
pub fn atomic_copy_dir(source: &Path, dest: &Path) -> Result<(), MarsError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp_dir = tempfile::TempDir::new_in(parent)?;
    copy_dir_following_symlinks(source, tmp_dir.path())?;
    let tmp_path = tmp_dir.keep();

    replace_generated_dir(&tmp_path, dest)
}

/// Remove a file or directory tree safely.
pub fn safe_remove(path: &Path) -> Result<(), MarsError> {
    platform_safe_remove(path)
}

/// Compute hash of file or directory for comparison.
pub fn content_hash(path: &Path, kind: ItemKind) -> Result<ContentHash, MarsError> {
    crate::hash::compute_hash(path, kind).map(ContentHash::from)
}

/// Recursively copy a directory, following symlinks on the source side.
///
/// Uses `fs::metadata` (not `symlink_metadata`) to follow symlinks.
/// Files are copied with plain `fs::read`+`fs::write` because the destination
/// is inside a temp dir — the atomicity guarantee comes from the final rename
/// of the enclosing temp dir, not from per-file atomics.
fn copy_dir_following_symlinks(source: &Path, dest: &Path) -> Result<(), MarsError> {
    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        // Follow symlinks — fs::metadata resolves through symlinks
        let metadata = match fs::metadata(&source_path) {
            Ok(m) => m,
            Err(e) => {
                // If it's a broken symlink, give a descriptive error
                if entry.file_type()?.is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("broken symlink in source tree: {}", source_path.display()),
                    )
                    .into());
                }
                return Err(e.into());
            }
        };

        if metadata.is_dir() {
            copy_dir_following_symlinks(&source_path, &dest_path)?;
        } else if metadata.is_file() {
            let content = fs::read(&source_path)?;
            fs::write(&dest_path, &content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest_path, fs::Permissions::from_mode(0o644))?;
            }
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported filesystem entry: {}", source_path.display()),
            )
            .into());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_copy_file_copies_regular_file() {
        let dir = TempDir::new().expect("temp dir");
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest").join("copied.txt");
        fs::write(&source, "hello").expect("write source");

        atomic_copy_file(&source, &dest).expect("copy file");

        assert_eq!(fs::read_to_string(dest).expect("read dest"), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_copy_file_follows_source_symlink() {
        let dir = TempDir::new().expect("temp dir");
        let real = dir.path().join("real.txt");
        fs::write(&real, "from-real").expect("write real");

        let source_link = dir.path().join("source-link.txt");
        std::os::unix::fs::symlink(&real, &source_link).expect("create symlink");

        let dest = dir.path().join("dest").join("copied.txt");
        atomic_copy_file(&source_link, &dest).expect("copy through symlink");

        let dest_meta = fs::symlink_metadata(&dest).expect("dest metadata");
        assert!(
            !dest_meta.file_type().is_symlink(),
            "dest should be a regular file"
        );
        assert_eq!(fs::read_to_string(dest).expect("read dest"), "from-real");
    }

    #[test]
    fn atomic_copy_dir_copies_tree() {
        let dir = TempDir::new().expect("temp dir");
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("nested")).expect("create source tree");
        fs::write(source.join("root.txt"), "root").expect("write root");
        fs::write(source.join("nested").join("child.txt"), "child").expect("write child");

        let dest = dir.path().join("dest");
        atomic_copy_dir(&source, &dest).expect("copy dir");

        assert_eq!(
            fs::read_to_string(dest.join("root.txt")).expect("read root"),
            "root"
        );
        assert_eq!(
            fs::read_to_string(dest.join("nested").join("child.txt")).expect("read child"),
            "child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_copy_dir_follows_symlinks() {
        let dir = TempDir::new().expect("temp dir");
        let shared = dir.path().join("shared");
        fs::create_dir_all(shared.join("docs")).expect("create shared tree");
        fs::write(shared.join("docs").join("guide.md"), "guide").expect("write guide");
        fs::write(shared.join("main.txt"), "main").expect("write main");

        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("create source");
        std::os::unix::fs::symlink(shared.join("main.txt"), source.join("main-link.txt"))
            .expect("file symlink");
        std::os::unix::fs::symlink(shared.join("docs"), source.join("docs-link"))
            .expect("dir symlink");

        let dest = dir.path().join("dest");
        atomic_copy_dir(&source, &dest).expect("copy dir through symlinks");

        let main_meta = fs::symlink_metadata(dest.join("main-link.txt")).expect("main metadata");
        assert!(
            !main_meta.file_type().is_symlink(),
            "copied file entry should be regular"
        );
        assert_eq!(
            fs::read_to_string(dest.join("main-link.txt")).expect("read copied main"),
            "main"
        );

        let docs_meta = fs::symlink_metadata(dest.join("docs-link")).expect("docs metadata");
        assert!(
            !docs_meta.file_type().is_symlink(),
            "copied dir entry should be regular directory"
        );
        assert_eq!(
            fs::read_to_string(dest.join("docs-link").join("guide.md")).expect("read guide"),
            "guide"
        );
    }
}
