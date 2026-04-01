use std::path::Path;

use crate::error::MarsError;
use crate::lock::ItemKind;

/// Compute SHA-256 of a file or directory (for skills).
///
/// For agents (single `.md` file): SHA-256 of file content.
/// For skills (directory): SHA-256 of sorted `(relative_path, file_hash)` pairs —
/// deterministic regardless of filesystem ordering.
pub fn compute_hash(path: &Path, kind: ItemKind) -> Result<String, MarsError> {
    let _ = (path, kind);
    todo!()
}
