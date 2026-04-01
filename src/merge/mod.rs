use crate::error::MarsError;

/// Result of a three-way merge via `git2::merge_file()`.
#[derive(Debug, Clone)]
pub struct MergeResult {
    /// The merged content (may contain conflict markers).
    pub content: Vec<u8>,
    /// Whether the merge produced conflict markers.
    pub has_conflicts: bool,
    /// Number of conflict regions.
    pub conflict_count: usize,
}

/// Labels for the three sides of a merge.
#[derive(Debug, Clone)]
pub struct MergeLabels {
    /// e.g., "base (mars installed)"
    pub base: String,
    /// e.g., "local"
    pub local: String,
    /// e.g., "meridian-base@v0.6.0"
    pub theirs: String,
}

/// Perform three-way merge using `git2::merge_file()`.
///
/// Inputs:
/// - `base`: what mars installed last time (from cache)
/// - `local`: current file on disk
/// - `theirs`: new source content
///
/// Output: merged content, possibly with git conflict markers.
pub fn merge_content(
    base: &[u8],
    local: &[u8],
    theirs: &[u8],
    labels: &MergeLabels,
) -> Result<MergeResult, MarsError> {
    let _ = (base, local, theirs, labels);
    todo!()
}
