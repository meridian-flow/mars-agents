use std::path::Path;

use crate::error::MarsError;
use crate::lock::ItemKind;

/// Atomic file write: write to temp file in same directory, then rename.
///
/// The rename is atomic on POSIX. Temp files are in the same directory
/// as the destination to guarantee same-filesystem atomic rename.
pub fn atomic_write(dest: &Path, content: &[u8]) -> Result<(), MarsError> {
    let _ = (dest, content);
    todo!()
}

/// Atomic directory install: copy to temp dir, then rename.
pub fn atomic_install_dir(src: &Path, dest: &Path) -> Result<(), MarsError> {
    let _ = (src, dest);
    todo!()
}

/// Remove a file or directory (skills are dirs).
pub fn remove_item(path: &Path, kind: ItemKind) -> Result<(), MarsError> {
    let _ = (path, kind);
    todo!()
}

/// Advisory file lock (flock) for concurrent access.
///
/// Prevents concurrent `mars sync` from corrupting state.
/// The lock is held start-to-end — acquired before fetching and held through completion.
pub struct FileLock {
    _fd: std::fs::File,
}

impl FileLock {
    /// Acquire an advisory file lock, blocking until available.
    pub fn acquire(lock_path: &Path) -> Result<Self, MarsError> {
        let _ = lock_path;
        todo!()
    }
}
