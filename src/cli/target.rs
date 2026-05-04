//! Shared target-name validation for `link` and `unlink` commands.

use crate::error::MarsError;

/// Normalize and validate a target directory name.
///
/// Strips trailing slashes, rejects paths (containing `/` or `\`),
/// and rejects empty/dot names.
pub fn normalize_target_name(target: &str) -> Result<String, MarsError> {
    let normalized = target.trim_end_matches('/').trim_end_matches('\\');
    if normalized.contains('/') || normalized.contains('\\') {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "target must be a directory name, not a path".to_string(),
        });
    }
    if normalized.is_empty() || normalized == "." || normalized == ".." {
        return Err(MarsError::Link {
            target: target.to_string(),
            message: "invalid target name".to_string(),
        });
    }
    Ok(normalized.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_target_name;

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize_target_name(".claude/").unwrap(), ".claude");
    }

    #[test]
    fn normalize_rejects_path() {
        assert!(normalize_target_name("foo/bar").is_err());
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_target_name("").is_err());
    }

    #[test]
    fn normalize_rejects_dots() {
        assert!(normalize_target_name(".").is_err());
        assert!(normalize_target_name("..").is_err());
    }
}
