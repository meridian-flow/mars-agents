use std::path::PathBuf;

/// Config-level errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    NotFound { path: PathBuf },

    #[error("invalid config: {message}")]
    Invalid { message: String },

    #[error("source `{name}` uses both agents/skills and exclude — pick one")]
    ConflictingFilters { name: String },

    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Lock file errors
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("lock file corrupt: {message}")]
    Corrupt { message: String },

    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolution errors
#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error("version conflict for `{name}`: {message}")]
    VersionConflict { name: String, message: String },

    #[error(
        "duplicate source identity: `{existing_name}` and `{duplicate_name}` both resolve to `{source_id}`"
    )]
    DuplicateSourceIdentity {
        existing_name: String,
        duplicate_name: String,
        source_id: String,
    },

    #[error(
        "source `{name}` was referenced with conflicting identities: existing `{existing}`, incoming `{incoming}`"
    )]
    SourceIdentityMismatch {
        name: String,
        existing: String,
        incoming: String,
    },

    #[error("cycle detected: {chain}")]
    Cycle { chain: String },

    #[error("source not found: {name}")]
    SourceNotFound { name: String },
}

/// Validation errors
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("unresolvable skill references found")]
    UnresolvableRefs,
}

/// Top-level error type aggregating all module errors
#[derive(Debug, thiserror::Error)]
pub enum MarsError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("lock error: {0}")]
    Lock(#[from] LockError),

    #[error("source error: {source_name}: {message}")]
    Source {
        source_name: String,
        message: String,
    },

    /// Sync refused to overwrite a file/directory not tracked in mars.lock.
    #[error("source error: {source_name}: refusing to overwrite unmanaged path `{}`", path.display())]
    UnmanagedCollision {
        source_name: String,
        path: PathBuf,
    },

    #[error("resolution failed: {0}")]
    Resolution(#[from] ResolutionError),

    #[error("merge conflict in {path}")]
    Conflict { path: String },

    #[error("{item} is provided by both `{source_a}` and `{source_b}`")]
    Collision {
        item: String,
        source_a: String,
        source_b: String,
    },

    #[error("validation: {0}")]
    Validation(#[from] ValidationError),

    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    #[error("frozen violation: {message}")]
    FrozenViolation { message: String },

    #[error(
        "locked commit {commit} is no longer reachable in {url} — the tag may have been force-pushed"
    )]
    LockedCommitUnreachable { commit: String, url: String },

    /// Link operation error — conflict, missing target, bad symlink.
    #[error("link error: {target}: {message}")]
    Link {
        target: String,
        message: String,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {url} — {status}: {message}")]
    Http {
        url: String,
        status: u16,
        message: String,
    },

    #[error("git command failed: `{command}` — {message}")]
    GitCli { command: String, message: String },
}

impl MarsError {
    /// Map error variants to CLI exit codes.
    ///
    /// - 1: sync completed with unresolved conflicts
    /// - 2: resolution/validation/config error
    /// - 3: source, I/O, HTTP, or git CLI error
    pub fn exit_code(&self) -> i32 {
        match self {
            MarsError::Conflict { .. } => 1,
            MarsError::Link { .. }
            | MarsError::Config(_)
            | MarsError::Lock(_)
            | MarsError::Resolution(_)
            | MarsError::Collision { .. }
            | MarsError::Validation(_)
            | MarsError::InvalidRequest { .. }
            | MarsError::FrozenViolation { .. }
            | MarsError::LockedCommitUnreachable { .. } => 2,
            MarsError::Source { .. }
            | MarsError::UnmanagedCollision { .. }
            | MarsError::Io(_)
            | MarsError::Http { .. }
            | MarsError::GitCli { .. } => 3,
        }
    }
}

pub type Result<T> = std::result::Result<T, MarsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mars_error_exit_codes_match_spec() {
        let cases = vec![
            (
                MarsError::Conflict {
                    path: "agents/reviewer.md".to_string(),
                },
                1,
            ),
            (
                MarsError::Config(ConfigError::Invalid {
                    message: "bad config".to_string(),
                }),
                2,
            ),
            (
                MarsError::Lock(LockError::Corrupt {
                    message: "bad lock".to_string(),
                }),
                2,
            ),
            (
                MarsError::Resolution(ResolutionError::SourceNotFound {
                    name: "missing".to_string(),
                }),
                2,
            ),
            (
                MarsError::Collision {
                    item: "coder".to_string(),
                    source_a: "base".to_string(),
                    source_b: "custom".to_string(),
                },
                2,
            ),
            (MarsError::Validation(ValidationError::UnresolvableRefs), 2),
            (
                MarsError::InvalidRequest {
                    message: "bad flag combination".to_string(),
                },
                2,
            ),
            (
                MarsError::FrozenViolation {
                    message: "lock file would change but --frozen is set".to_string(),
                },
                2,
            ),
            (
                MarsError::LockedCommitUnreachable {
                    commit: "abc123".to_string(),
                    url: "https://example.com/repo.git".to_string(),
                },
                2,
            ),
            (
                MarsError::Link {
                    target: ".claude".to_string(),
                    message: "conflicts found".to_string(),
                },
                2,
            ),
            (
                MarsError::Source {
                    source_name: "origin".to_string(),
                    message: "network failed".to_string(),
                },
                3,
            ),
            (
                MarsError::UnmanagedCollision {
                    source_name: "origin".to_string(),
                    path: PathBuf::from("agents/coder.md"),
                },
                3,
            ),
            (
                MarsError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                )),
                3,
            ),
            (
                MarsError::Http {
                    url: "https://example.com/archive.tar.gz".to_string(),
                    status: 503,
                    message: "service unavailable".to_string(),
                },
                3,
            ),
            (
                MarsError::GitCli {
                    command: "git ls-remote --tags https://example.com/repo".to_string(),
                    message: "fatal: repository not found".to_string(),
                },
                3,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(
                err.exit_code(),
                expected,
                "unexpected exit code for error: {err}"
            );
        }
    }
}
