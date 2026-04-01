//! Three-way merge using `git merge-file` CLI.
//!
//! Wraps `git merge-file -p` to produce git-standard conflict markers
//! that IDEs (VS Code, JetBrains) recognize and provide "Accept Current/
//! Incoming/Both" UI for.
//!
//! Uses `git merge-file` via subprocess because git2 0.19 does not expose
//! `git_merge_file()` at the Rust level. Since mars is inherently a git-based
//! tool, `git` being in PATH is a safe assumption.

use std::io::Write;
use std::process::Command;

use crate::error::MarsError;

/// Result of a three-way merge via `git merge-file`.
#[derive(Debug, Clone)]
pub struct MergeResult {
    /// The merged content (may contain conflict markers).
    pub content: Vec<u8>,
    /// Whether the merge produced conflict markers.
    pub has_conflicts: bool,
    /// Number of conflict regions (approximate — counts `<<<<<<<` markers).
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

/// Perform three-way merge using `git merge-file`.
///
/// Inputs:
/// - `base`: what mars installed last time (from cache)
/// - `local`: current file on disk (user's copy)
/// - `theirs`: new source content (upstream update)
///
/// Output: merged content, possibly with git conflict markers.
///
/// `git merge-file` exit codes:
/// - 0 = clean merge
/// - positive = number of conflicts
/// - negative = error
pub fn merge_content(
    base: &[u8],
    local: &[u8],
    theirs: &[u8],
    labels: &MergeLabels,
) -> Result<MergeResult, MarsError> {
    let dir = tempfile::TempDir::new()?;

    let base_path = dir.path().join("base");
    let local_path = dir.path().join("local");
    let theirs_path = dir.path().join("theirs");

    write_file(&base_path, base)?;
    write_file(&local_path, local)?;
    write_file(&theirs_path, theirs)?;

    // git merge-file -p -L <local-label> -L <base-label> -L <theirs-label>
    //   <local-file> <base-file> <theirs-file>
    //
    // Note: label order is local, base, theirs (matching file order).
    // The -p flag writes merged output to stdout instead of modifying the file.
    let output = Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg("-L")
        .arg(&labels.local)
        .arg("-L")
        .arg(&labels.base)
        .arg("-L")
        .arg(&labels.theirs)
        .arg(&local_path)
        .arg(&base_path)
        .arg(&theirs_path)
        .output()
        .map_err(|e| MarsError::Source {
            source_name: "merge".to_string(),
            message: format!(
                "failed to run `git merge-file`: {e} — is git installed and in PATH?"
            ),
        })?;

    let exit_code = output.status.code().unwrap_or(-1);

    // Negative exit code = error (not a conflict)
    if exit_code < 0 {
        return Err(MarsError::Source {
            source_name: "merge".to_string(),
            message: format!(
                "git merge-file failed (exit {}): {}",
                exit_code,
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    let content = output.stdout;
    let has_conflicts = exit_code > 0;
    let conflict_count = count_conflict_markers(&content);

    Ok(MergeResult {
        content,
        has_conflicts,
        conflict_count,
    })
}

/// Check if file content contains unresolved conflict markers.
///
/// Scans for `<<<<<<<` markers that indicate an unresolved merge conflict.
pub fn has_conflict_markers(content: &[u8]) -> bool {
    // Look for "<<<<<<< " at the start of a line
    if content.starts_with(b"<<<<<<<") {
        return true;
    }
    content
        .windows(8)
        .any(|w| w[0] == b'\n' && &w[1..] == b"<<<<<<<")
}

/// Count conflict marker regions in content.
fn count_conflict_markers(content: &[u8]) -> usize {
    let mut count = 0;

    // Check if content starts with a marker
    if content.len() >= 7 && &content[..7] == b"<<<<<<<" {
        count += 1;
    }

    // Count occurrences of "\n<<<<<<<" (marker at start of line)
    for window in content.windows(8) {
        if window[0] == b'\n' && &window[1..] == b"<<<<<<<" {
            count += 1;
        }
    }

    count
}

/// Helper to write bytes to a file.
fn write_file(path: &std::path::Path, content: &[u8]) -> Result<(), MarsError> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> MergeLabels {
        MergeLabels {
            base: "base (last sync)".to_string(),
            local: "local".to_string(),
            theirs: "meridian-base@v0.6.0".to_string(),
        }
    }

    // === Clean merge tests ===

    #[test]
    fn all_three_identical() {
        let content = b"line 1\nline 2\nline 3\n";
        let result = merge_content(content, content, content, &labels()).unwrap();
        assert!(!result.has_conflicts);
        assert_eq!(result.conflict_count, 0);
        assert_eq!(result.content, content);
    }

    #[test]
    fn theirs_changed_local_same_as_base() {
        let base = b"line 1\nline 2\nline 3\n";
        let local = b"line 1\nline 2\nline 3\n";
        let theirs = b"line 1\nline 2 modified\nline 3\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(!result.has_conflicts);
        assert_eq!(result.content, theirs);
    }

    #[test]
    fn local_changed_theirs_same_as_base() {
        let base = b"line 1\nline 2\nline 3\n";
        let local = b"line 1\nline 2 local edit\nline 3\n";
        let theirs = b"line 1\nline 2\nline 3\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(!result.has_conflicts);
        assert_eq!(result.content, local);
    }

    #[test]
    fn non_overlapping_changes_merge_cleanly() {
        let base = b"line 1\nline 2\nline 3\nline 4\nline 5\n";
        let local = b"line 1 local\nline 2\nline 3\nline 4\nline 5\n";
        let theirs = b"line 1\nline 2\nline 3\nline 4\nline 5 theirs\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(!result.has_conflicts);
        let merged = String::from_utf8(result.content).unwrap();
        assert!(merged.contains("line 1 local"));
        assert!(merged.contains("line 5 theirs"));
    }

    // === Conflict tests ===

    #[test]
    fn overlapping_changes_produce_conflict() {
        let base = b"line 1\nline 2\nline 3\n";
        let local = b"line 1\nlocal change\nline 3\n";
        let theirs = b"line 1\ntheirs change\nline 3\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(result.has_conflicts);
        assert!(result.conflict_count >= 1);
    }

    #[test]
    fn conflict_markers_match_git_format() {
        let base = b"same\nconflict line\nsame\n";
        let local = b"same\nlocal version\nsame\n";
        let theirs = b"same\ntheirs version\nsame\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(result.has_conflicts);

        let merged = String::from_utf8(result.content).unwrap();
        assert!(merged.contains("<<<<<<<"), "should have opening marker");
        assert!(merged.contains("======="), "should have separator");
        assert!(merged.contains(">>>>>>>"), "should have closing marker");
    }

    #[test]
    fn labels_appear_in_conflict_markers() {
        let base = b"conflict\n";
        let local = b"local version\n";
        let theirs = b"theirs version\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        let merged = String::from_utf8(result.content).unwrap();
        assert!(
            merged.contains("local"),
            "local label should appear: {merged}"
        );
        assert!(
            merged.contains("meridian-base@v0.6.0"),
            "theirs label should appear: {merged}"
        );
    }

    #[test]
    fn multiple_conflict_regions() {
        // Use more spacing between conflicting regions so git treats them separately
        let base = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let local = b"a-local\nb\nc\nd\ne\nf\ng\nh\ni-local\nj\n";
        let theirs = b"a-theirs\nb\nc\nd\ne\nf\ng\nh\ni-theirs\nj\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(result.has_conflicts);
        assert!(
            result.conflict_count >= 2,
            "should have at least 2 conflicts, got {}",
            result.conflict_count
        );
    }

    // === Edge cases ===

    #[test]
    fn empty_base_with_different_content() {
        let base = b"";
        let local = b"local content\n";
        let theirs = b"theirs content\n";

        // Empty base with both sides adding content → conflict
        let result = merge_content(base, local, theirs, &labels()).unwrap();
        // Both added content from empty base — this is a conflict
        assert!(result.has_conflicts);
    }

    #[test]
    fn empty_base_same_additions() {
        let base = b"";
        let local = b"same content\n";
        let theirs = b"same content\n";

        let result = merge_content(base, local, theirs, &labels()).unwrap();
        assert!(!result.has_conflicts);
        assert_eq!(result.content, b"same content\n");
    }

    #[test]
    fn all_empty() {
        let result = merge_content(b"", b"", b"", &labels()).unwrap();
        assert!(!result.has_conflicts);
        assert!(result.content.is_empty());
    }

    // === has_conflict_markers tests ===

    #[test]
    fn has_conflict_markers_detects_markers() {
        let content = b"before\n<<<<<<< local\nlocal\n=======\ntheirs\n>>>>>>> theirs\nafter\n";
        assert!(has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_at_start_of_file() {
        let content = b"<<<<<<< local\nlocal\n=======\ntheirs\n>>>>>>> theirs\n";
        assert!(has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_no_markers() {
        let content = b"normal content\nno conflicts here\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_partial_marker_not_detected() {
        // "<<<<<<" (6 chars) shouldn't be detected — needs 7 (`<<<<<<<`)
        let content = b"some <<<<<< stuff\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_in_middle_of_line_not_detected() {
        // Marker must be at start of line
        let content = b"text <<<<<<< not a real marker\n";
        assert!(!has_conflict_markers(content));
    }

    // === count_conflict_markers tests ===

    #[test]
    fn count_zero_conflicts() {
        assert_eq!(count_conflict_markers(b"no conflicts"), 0);
    }

    #[test]
    fn count_one_conflict() {
        let content =
            b"before\n<<<<<<< local\nlocal\n=======\ntheirs\n>>>>>>> theirs\nafter\n";
        assert_eq!(count_conflict_markers(content), 1);
    }

    #[test]
    fn count_multiple_conflicts() {
        let content = b"<<<<<<< a\nx\n=======\ny\n>>>>>>> b\nok\n<<<<<<< a\np\n=======\nq\n>>>>>>> b\n";
        assert_eq!(count_conflict_markers(content), 2);
    }
}
