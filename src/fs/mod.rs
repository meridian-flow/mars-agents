use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::MarsError;
use crate::types::ItemKind;

/// Top-level source entries excluded when installing flat skill repositories.
pub const FLAT_SKILL_EXCLUDED_TOP_LEVEL: &[&str] = &[
    ".git",
    ".mars",
    "mars.toml",
    "mars.lock",
    "mars.local.toml",
    ".gitignore",
];

/// Atomic file write: write to temp file in same directory, then rename.
///
/// The rename is atomic on POSIX. Temp files are in the same directory
/// as the destination to guarantee same-filesystem atomic rename.
pub fn atomic_write(dest: &Path, content: &[u8]) -> Result<(), MarsError> {
    // Ensure parent directory exists
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

/// Atomic directory install: copy source tree to a temp dir in the same
/// parent as `dest`, then rename into place.
///
/// Uses rename-old-then-rename-new to minimize the window where `dest`
/// doesn't exist. If `dest` already exists, it's renamed to `.{name}.old`
/// before the new content takes its place. Stale `.old` from prior crashes
/// is cleaned up automatically.
pub fn atomic_install_dir(src: &Path, dest: &Path) -> Result<(), MarsError> {
    atomic_install_dir_impl(src, dest, &[])
}

/// Atomic directory install with optional top-level source entry exclusions.
pub fn atomic_install_dir_filtered(
    src: &Path,
    dest: &Path,
    excluded_top_level: &[&str],
) -> Result<(), MarsError> {
    atomic_install_dir_impl(src, dest, excluded_top_level)
}

fn atomic_install_dir_impl(
    src: &Path,
    dest: &Path,
    excluded_top_level: &[&str],
) -> Result<(), MarsError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp_dir = tempfile::TempDir::new_in(parent)?;
    copy_dir_recursive(src, tmp_dir.path(), src, excluded_top_level)?;
    let tmp_path = tmp_dir.keep();

    if dest.exists() {
        // Step 1: Rename old to .old (old content still accessible)
        let old_path = parent.join(format!(
            ".{}.old",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        // Clean up stale .old from a prior crash
        if old_path.exists() {
            fs::remove_dir_all(&old_path)?;
        }
        // Atomic: old content moves to .old, dest slot is free
        fs::rename(dest, &old_path)?;
        // Atomic: new content takes dest slot
        if let Err(e) = fs::rename(&tmp_path, dest) {
            // Rollback: move old content back
            let _ = fs::rename(&old_path, dest);
            let _ = fs::remove_dir_all(&tmp_path);
            return Err(e.into());
        }
        // Cleanup: remove old content (non-critical)
        let _ = fs::remove_dir_all(&old_path);
    } else {
        fs::rename(&tmp_path, dest)?;
    }

    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(
    src: &Path,
    dest: &Path,
    root: &Path,
    excluded_top_level: &[&str],
) -> Result<(), MarsError> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        let rel_path = src_path
            .strip_prefix(root)
            .expect("copy traversal path should be under root");
        if is_excluded_top_level(rel_path, excluded_top_level) {
            continue;
        }

        if file_type.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_dir_recursive(&src_path, &dest_path, root, excluded_top_level)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

fn is_excluded_top_level(path: &Path, excluded_top_level: &[&str]) -> bool {
    let Some(first) = path.components().next().map(|c| c.as_os_str()) else {
        return false;
    };
    excluded_top_level.iter().any(|excluded| first == *excluded)
}

/// Remove a file or directory (skills are dirs).
pub fn remove_item(path: &Path, kind: ItemKind) -> Result<(), MarsError> {
    match kind {
        ItemKind::Agent => fs::remove_file(path)?,
        ItemKind::Skill => fs::remove_dir_all(path)?,
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
pub fn clear_readonly(path: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut perms = metadata.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            std::fs::set_permissions(path, perms)?;
        }
    }
    Ok(())
}

/// Advisory file lock (flock) for concurrent access.
///
/// Prevents concurrent `mars sync` from corrupting state.
/// The lock is held start-to-end — acquired before fetching and held through completion.
/// Dropping the `FileLock` closes the fd, which releases the advisory lock.
pub struct FileLock {
    _fd: fs::File,
}

impl FileLock {
    /// Acquire an advisory file lock, blocking until available.
    pub fn acquire(lock_path: &Path) -> Result<Self, MarsError> {
        let file = Self::open_lock_file(lock_path)?;
        platform::lock_exclusive(&file)?;
        Ok(FileLock { _fd: file })
    }

    /// Try to acquire the lock without blocking.
    /// Returns `Ok(Some(lock))` if acquired, `Ok(None)` if already held by another process.
    pub fn try_acquire(lock_path: &Path) -> Result<Option<Self>, MarsError> {
        let file = Self::open_lock_file(lock_path)?;
        match platform::try_lock_exclusive(&file) {
            Ok(true) => Ok(Some(FileLock { _fd: file })),
            Ok(false) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Open (or create) the lock file, creating parent dirs if needed.
    fn open_lock_file(lock_path: &Path) -> Result<fs::File, MarsError> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        Ok(file)
    }
}

#[cfg(unix)]
mod platform {
    use std::fs;
    use std::os::unix::io::AsRawFd;

    pub fn lock_exclusive(file: &fs::File) -> std::io::Result<()> {
        // SAFETY: the file descriptor is valid while `file` is alive.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn try_lock_exclusive(file: &fs::File) -> std::io::Result<bool> {
        // SAFETY: the file descriptor is valid while `file` is alive.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(err)
            }
        } else {
            Ok(true)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };

    const ERROR_LOCK_VIOLATION: i32 = 33;

    pub fn lock_exclusive(file: &fs::File) -> std::io::Result<()> {
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: zero-initialized OVERLAPPED is accepted by LockFileEx for
        // whole-file locks at offset 0.
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: handle is valid while `file` is alive and `overlapped` outlives the call.
        let ret =
            unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, !0, !0, &mut overlapped) };
        if ret == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn try_lock_exclusive(file: &fs::File) -> std::io::Result<bool> {
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: zero-initialized OVERLAPPED is accepted by LockFileEx for
        // whole-file locks at offset 0.
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: handle is valid while `file` is alive and `overlapped` outlives the call.
        let ret = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                !0,
                !0,
                &mut overlapped,
            )
        };
        if ret == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
                Ok(false)
            } else {
                Err(err)
            }
        } else {
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_creates_file_with_correct_content() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("output.txt");
        let content = b"hello world";

        atomic_write(&dest, content).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("nested").join("dir").join("file.txt");
        let content = b"nested content";

        atomic_write(&dest, content).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("output.txt");

        atomic_write(&dest, b"first").unwrap();
        atomic_write(&dest, b"second").unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"second");
    }

    #[test]
    fn atomic_install_dir_copies_tree() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        // Create source tree
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), "file a").unwrap();
        fs::write(src.join("sub").join("b.txt"), "file b").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "file a");
        assert_eq!(
            fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
            "file b"
        );
    }

    #[test]
    fn atomic_install_dir_replaces_existing() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        // Create initial dest
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("old.txt"), "old").unwrap();

        // Create source
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), "new").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("new.txt").exists());
        assert!(!dest.join("old.txt").exists());
    }

    #[test]
    fn atomic_install_dir_cleans_stale_old() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        // Create initial dest
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("old.txt"), "old").unwrap();

        // Create stale .old from a prior crash
        let old_path = dir.path().join(".dest_dir.old");
        fs::create_dir_all(&old_path).unwrap();
        fs::write(old_path.join("stale.txt"), "stale").unwrap();

        // Create source
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("new.txt"), "new").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("new.txt").exists());
        assert!(!dest.join("old.txt").exists());
        assert!(!old_path.exists(), "stale .old should be cleaned up");
    }

    #[test]
    fn atomic_install_dir_dest_exists_throughout() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        // Create initial dest
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("v1.txt"), "v1").unwrap();

        // Create source
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("v2.txt"), "v2").unwrap();

        assert!(dest.exists(), "dest should exist before install");
        atomic_install_dir(&src, &dest).unwrap();
        assert!(dest.exists(), "dest should exist after install");
        assert!(dest.join("v2.txt").exists());
    }

    #[test]
    fn atomic_install_dir_filtered_excludes_top_level_entries() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");

        fs::create_dir_all(src.join(".git")).unwrap();
        fs::create_dir_all(src.join("resources")).unwrap();
        fs::write(src.join("SKILL.md"), "skill").unwrap();
        fs::write(src.join("mars.toml"), "ignored").unwrap();
        fs::write(src.join(".gitignore"), "ignored").unwrap();
        fs::write(src.join(".git").join("config"), "ignored").unwrap();
        fs::write(src.join("resources").join("guide.md"), "kept").unwrap();

        atomic_install_dir_filtered(&src, &dest, FLAT_SKILL_EXCLUDED_TOP_LEVEL).unwrap();

        assert!(dest.join("SKILL.md").exists());
        assert!(dest.join("resources").join("guide.md").exists());
        assert!(!dest.join(".git").exists());
        assert!(!dest.join("mars.toml").exists());
        assert!(!dest.join(".gitignore").exists());
    }

    #[test]
    fn remove_item_removes_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("agent.md");
        fs::write(&file, "agent content").unwrap();

        remove_item(&file, ItemKind::Agent).unwrap();

        assert!(!file.exists());
    }

    #[test]
    fn remove_item_removes_directory() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(skill_dir.join("sub")).unwrap();
        fs::write(skill_dir.join("main.md"), "skill").unwrap();
        fs::write(skill_dir.join("sub").join("helper.md"), "helper").unwrap();

        remove_item(&skill_dir, ItemKind::Skill).unwrap();

        assert!(!skill_dir.exists());
    }

    #[test]
    fn file_lock_acquire_returns_lock() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");

        let lock = FileLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
        drop(lock);
    }

    #[test]
    fn file_lock_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");

        {
            let _lock = FileLock::acquire(&lock_path).unwrap();
            // Lock held here
        }
        // Lock dropped — should be acquirable again
        let lock2 = FileLock::try_acquire(&lock_path).unwrap();
        assert!(lock2.is_some());
    }

    #[test]
    fn file_lock_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("nested").join("dir").join("test.lock");

        let lock = FileLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
        drop(lock);
    }
}
