//! Source string syntax classification.
//!
//! Determines whether a CLI/config source string is a local path or a source identifier.

use std::path::PathBuf;

/// Classify a source string as a local path if it matches local path syntax.
///
/// Accepted POSIX forms:
/// - `/absolute`
/// - `./relative`, `../relative`
/// - `~/home-relative`
/// - `.` and `..`
///
/// Accepted Windows forms:
/// - Drive paths: `C:\path`, `C:/path`, `C:relative`
/// - UNC paths: `\\server\share\path`
/// - Root-relative: `\path`
/// - Relative backslash: `.\path`, `..\path`, `foo\bar`
///
/// Returns `Some(PathBuf)` if the input is a local path, `None` otherwise.
///
/// NOT classified as local paths (URL-like or shorthand):
/// - Contains `://` (URL scheme)
/// - SSH shorthand: `git@host:path`
/// - GitHub/GitLab shorthand: `owner/repo` (no backslash, has forward slash)
/// - Protocol aliases: `github:`, `gitlab:`
pub fn classify_local_source(input: &str) -> Option<PathBuf> {
    // Empty input is not a local path.
    if input.is_empty() {
        return None;
    }

    // URL-like inputs are never local paths.
    if input.contains("://") {
        return None;
    }

    // SSH shorthand (git@host:path) is not a local path.
    if is_ssh_shorthand(input) {
        return None;
    }

    // Protocol aliases are not local paths.
    if input.starts_with("github:") || input.starts_with("gitlab:") {
        return None;
    }

    // POSIX absolute path.
    if input.starts_with('/') {
        return Some(PathBuf::from(input));
    }

    // POSIX relative (current dir, parent dir).
    if input == "." || input == ".." || input.starts_with("./") || input.starts_with("../") {
        return Some(PathBuf::from(input));
    }

    // Home directory expansion.
    if input.starts_with('~') {
        return Some(PathBuf::from(input));
    }

    // Windows drive path: C:\, C:/, C:relative.
    if is_windows_drive_path(input) {
        return Some(PathBuf::from(input));
    }

    // Windows UNC path: \\server\share.
    if input.starts_with("\\\\") {
        // Reject extended/device paths.
        if input.starts_with("\\\\?\\") || input.starts_with("\\\\.\\") {
            return None;
        }
        return Some(PathBuf::from(input));
    }

    // Windows root-relative: \path.
    if input.starts_with('\\') {
        return Some(PathBuf::from(input));
    }

    // Windows relative with backslash: .\path, ..\path.
    if input.starts_with(".\\") || input.starts_with("..\\") {
        return Some(PathBuf::from(input));
    }

    // Contains backslash but no slash - treat as Windows relative path.
    if input.contains('\\') && !input.contains('/') {
        return Some(PathBuf::from(input));
    }

    // Forward-slash-only owner/repo-style shorthand is not a local path.
    if input.contains('/') && !input.contains('\\') && !input.contains('.') {
        return None;
    }

    None
}

/// Check if input is a Windows drive path (C:\, C:/, C:relative).
fn is_windows_drive_path(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Check if input is SSH shorthand (git@host:path).
fn is_ssh_shorthand(input: &str) -> bool {
    if !input.contains('@') || !input.contains(':') {
        return false;
    }

    match (input.find('@'), input.find(':')) {
        (Some(at), Some(colon)) => at < colon && colon + 1 < input.len(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::classify_local_source;

    #[test]
    fn classify_posix_absolute() {
        assert!(classify_local_source("/absolute/path").is_some());
        assert!(classify_local_source("/").is_some());
    }

    #[test]
    fn classify_posix_relative() {
        assert!(classify_local_source(".").is_some());
        assert!(classify_local_source("..").is_some());
        assert!(classify_local_source("./relative").is_some());
        assert!(classify_local_source("../parent").is_some());
    }

    #[test]
    fn classify_home_relative() {
        assert!(classify_local_source("~/path").is_some());
        assert!(classify_local_source("~").is_some());
    }

    #[test]
    fn classify_windows_drive_paths() {
        assert!(classify_local_source("C:\\path").is_some());
        assert!(classify_local_source("C:/path").is_some());
        assert!(classify_local_source("D:\\").is_some());
        assert!(classify_local_source("C:relative").is_some());
    }

    #[test]
    fn classify_windows_unc_paths() {
        assert!(classify_local_source("\\\\server\\share").is_some());
        assert!(classify_local_source("\\\\server\\share\\path").is_some());
    }

    #[test]
    fn classify_windows_root_relative() {
        assert!(classify_local_source("\\path").is_some());
    }

    #[test]
    fn classify_windows_backslash_relative() {
        assert!(classify_local_source(".\\relative").is_some());
        assert!(classify_local_source("..\\parent").is_some());
        assert!(classify_local_source("foo\\bar").is_some());
    }

    #[test]
    fn classify_rejects_extended_paths() {
        assert!(classify_local_source("\\\\?\\C:\\path").is_none());
        assert!(classify_local_source("\\\\.\\Device").is_none());
    }

    #[test]
    fn classify_rejects_urls() {
        assert!(classify_local_source("https://github.com/owner/repo").is_none());
        assert!(classify_local_source("git://host/path").is_none());
        assert!(classify_local_source("ssh://git@host/path").is_none());
    }

    #[test]
    fn classify_rejects_ssh_shorthand() {
        assert!(classify_local_source("git@github.com:owner/repo").is_none());
        assert!(classify_local_source("git@host:path").is_none());
    }

    #[test]
    fn classify_rejects_protocol_aliases() {
        assert!(classify_local_source("github:owner/repo").is_none());
        assert!(classify_local_source("gitlab:group/repo").is_none());
    }

    #[test]
    fn classify_rejects_shorthand() {
        assert!(classify_local_source("owner/repo").is_none());
        assert!(classify_local_source("owner/repo/subpath").is_none());
    }
}
