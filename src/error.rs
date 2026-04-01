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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("git error: {0}")]
    Git(#[from] git2::Error),
}

impl MarsError {
    /// Map error variants to CLI exit codes.
    ///
    /// - 1: sync completed with unresolved conflicts
    /// - 2: resolution/validation/config error
    /// - 3: I/O or git error
    pub fn exit_code(&self) -> i32 {
        match self {
            MarsError::Conflict { .. } => 1,
            MarsError::Config(_)
            | MarsError::Lock(_)
            | MarsError::Resolution(_)
            | MarsError::Collision { .. }
            | MarsError::Validation(_)
            | MarsError::InvalidRequest { .. }
            | MarsError::FrozenViolation { .. }
            | MarsError::LockedCommitUnreachable { .. } => 2,
            MarsError::Source { .. } | MarsError::Io(_) | MarsError::Git(_) => 3,
        }
    }
}

pub type Result<T> = std::result::Result<T, MarsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mars_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let mars_err: MarsError = io_err.into();
        assert!(matches!(mars_err, MarsError::Io(_)));
        assert!(mars_err.to_string().contains("file missing"));
    }

    #[test]
    fn mars_error_from_config_error() {
        let cfg_err = ConfigError::NotFound {
            path: PathBuf::from("/missing/agents.toml"),
        };
        let mars_err: MarsError = cfg_err.into();
        assert!(matches!(mars_err, MarsError::Config(_)));
        assert!(mars_err.to_string().contains("/missing/agents.toml"));
    }

    #[test]
    fn mars_error_from_lock_error() {
        let lock_err = LockError::Corrupt {
            message: "unexpected EOF".to_string(),
        };
        let mars_err: MarsError = lock_err.into();
        assert!(matches!(mars_err, MarsError::Lock(_)));
        assert!(mars_err.to_string().contains("unexpected EOF"));
    }

    #[test]
    fn mars_error_from_resolution_error() {
        let res_err = ResolutionError::SourceNotFound {
            name: "missing-source".to_string(),
        };
        let mars_err: MarsError = res_err.into();
        assert!(matches!(mars_err, MarsError::Resolution(_)));
        assert!(mars_err.to_string().contains("missing-source"));
    }

    #[test]
    fn mars_error_from_validation_error() {
        let val_err = ValidationError::UnresolvableRefs;
        let mars_err: MarsError = val_err.into();
        assert!(matches!(mars_err, MarsError::Validation(_)));
        assert!(mars_err.to_string().contains("unresolvable"));
    }

    #[test]
    fn source_error_formats_correctly() {
        let err = MarsError::Source {
            source_name: "my-source".to_string(),
            message: "fetch failed".to_string(),
        };
        assert_eq!(err.to_string(), "source error: my-source: fetch failed");
    }

    #[test]
    fn conflict_error_formats_correctly() {
        let err = MarsError::Conflict {
            path: "agents/reviewer.md".to_string(),
        };
        assert_eq!(err.to_string(), "merge conflict in agents/reviewer.md");
    }

    #[test]
    fn collision_error_formats_correctly() {
        let err = MarsError::Collision {
            item: "reviewer".to_string(),
            source_a: "base".to_string(),
            source_b: "custom".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "reviewer is provided by both `base` and `custom`"
        );
    }

    #[test]
    fn config_error_conflicting_filters() {
        let err = ConfigError::ConflictingFilters {
            name: "my-source".to_string(),
        };
        assert!(err.to_string().contains("my-source"));
        assert!(err.to_string().contains("agents/skills and exclude"));
    }

    #[test]
    fn resolution_version_conflict() {
        let err = ResolutionError::VersionConflict {
            name: "pkg".to_string(),
            message: "1.0 vs 2.0".to_string(),
        };
        assert!(err.to_string().contains("pkg"));
        assert!(err.to_string().contains("1.0 vs 2.0"));
    }

    #[test]
    fn resolution_cycle() {
        let err = ResolutionError::Cycle {
            chain: "a → b → a".to_string(),
        };
        assert!(err.to_string().contains("a → b → a"));
    }

    #[test]
    fn result_alias_works() {
        fn example() -> Result<i32> {
            Ok(42)
        }
        assert_eq!(example().unwrap(), 42);
    }

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
                MarsError::Source {
                    source_name: "origin".to_string(),
                    message: "network failed".to_string(),
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
            (MarsError::Git(git2::Error::from_str("git failed")), 3),
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
