use std::path::PathBuf;

/// Config-level errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    NotFound { path: PathBuf },

    #[error(
        "no mars.toml found from {} to filesystem root. Run `mars init` first.",
        start.display()
    )]
    ProjectRootNotFound { start: PathBuf },

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
        "version conflict for item `{item}` from package `{package}`: {existing} vs {requested} (requester chain: {chain})"
    )]
    ItemVersionConflict {
        item: String,
        package: String,
        existing: String,
        requested: String,
        chain: String,
    },

    #[error(
        "package version conflict for `{package}`: {existing} vs {requested} (requester chain: {chain})"
    )]
    PackageVersionConflict {
        package: String,
        existing: String,
        requested: String,
        chain: String,
    },

    #[error(
        "skill `{skill}` not found (required by {required_by}; searched packages: {searched:?})"
    )]
    SkillNotFound {
        skill: String,
        required_by: String,
        searched: Vec<String>,
    },

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

    #[error(
        "source error: {source_name}: subpath `{subpath}` escapes checkout root `{}`",
        checkout_root.display()
    )]
    SubpathTraversal {
        source_name: String,
        subpath: String,
        checkout_root: PathBuf,
    },

    #[error(
        "source error: {source_name}: subpath `{subpath}` does not exist under checkout root `{}`",
        checkout_root.display()
    )]
    SubpathMissing {
        source_name: String,
        subpath: String,
        checkout_root: PathBuf,
    },

    #[error(
        "source error: {source_name}: subpath `{subpath}` is not a directory under checkout root `{}`",
        checkout_root.display()
    )]
    SubpathNotDirectory {
        source_name: String,
        subpath: String,
        checkout_root: PathBuf,
    },

    #[error(
        "discovery collision in `{source_name}`: {kind} `{item_name}` found at `{}` and `{}`",
        path_a.display(),
        path_b.display()
    )]
    DiscoveryCollision {
        source_name: String,
        kind: String,
        item_name: String,
        path_a: PathBuf,
        path_b: PathBuf,
    },

    #[error(
        "source error: {source_name}: plugin manifest path `{manifest_path}` escapes package root `{}`",
        package_root.display()
    )]
    ManifestDeclaredPathEscape {
        source_name: String,
        manifest_path: String,
        package_root: PathBuf,
    },

    #[error(
        "source error: {source_name}: plugin manifest path `{manifest_path}` does not exist under package root `{}`",
        package_root.display()
    )]
    ManifestDeclaredPathMissing {
        source_name: String,
        manifest_path: String,
        package_root: PathBuf,
    },

    /// Sync refused to overwrite a file/directory not tracked in mars.lock.
    #[error("source error: {source_name}: refusing to overwrite unmanaged path `{}`", path.display())]
    UnmanagedCollision { source_name: String, path: PathBuf },

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

    /// Link operation error — conflict, missing target, or invalid link metadata.
    #[error("link error: {target}: {message}")]
    Link { target: String, message: String },

    #[error(
        "models cache is empty and cannot be refreshed: {reason}. Run `mars models refresh` to populate it."
    )]
    ModelCacheUnavailable { reason: String },

    #[error("{operation} failed for {}: {source}", path.display())]
    Io {
        operation: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("HTTP error: {url} — {status}: {message}")]
    Http {
        url: String,
        status: u16,
        message: String,
    },

    #[error("git command failed: `{command}` — {message}")]
    GitCli { command: String, message: String },

    #[error("internal error: {0}")]
    Internal(String),
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
            | MarsError::SubpathTraversal { .. }
            | MarsError::SubpathMissing { .. }
            | MarsError::SubpathNotDirectory { .. }
            | MarsError::DiscoveryCollision { .. }
            | MarsError::ManifestDeclaredPathEscape { .. }
            | MarsError::ManifestDeclaredPathMissing { .. }
            | MarsError::UnmanagedCollision { .. }
            | MarsError::ModelCacheUnavailable { .. }
            | MarsError::Io { .. }
            | MarsError::Http { .. }
            | MarsError::GitCli { .. }
            | MarsError::Internal(_) => 3,
        }
    }
}

impl From<std::io::Error> for MarsError {
    fn from(source: std::io::Error) -> Self {
        MarsError::Io {
            operation: "I/O operation".to_string(),
            path: PathBuf::from("<unknown>"),
            source,
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
                MarsError::SubpathTraversal {
                    source_name: "origin".to_string(),
                    subpath: "../escape".to_string(),
                    checkout_root: PathBuf::from("/tmp/root"),
                },
                3,
            ),
            (
                MarsError::SubpathMissing {
                    source_name: "origin".to_string(),
                    subpath: "plugins/foo".to_string(),
                    checkout_root: PathBuf::from("/tmp/root"),
                },
                3,
            ),
            (
                MarsError::SubpathNotDirectory {
                    source_name: "origin".to_string(),
                    subpath: "plugins/foo".to_string(),
                    checkout_root: PathBuf::from("/tmp/root"),
                },
                3,
            ),
            (
                MarsError::DiscoveryCollision {
                    source_name: "origin".to_string(),
                    kind: "skill".to_string(),
                    item_name: "plan".to_string(),
                    path_a: PathBuf::from("skills/a"),
                    path_b: PathBuf::from("skills/b"),
                },
                3,
            ),
            (
                MarsError::ManifestDeclaredPathEscape {
                    source_name: "origin".to_string(),
                    manifest_path: "./../escape".to_string(),
                    package_root: PathBuf::from("/tmp/root"),
                },
                3,
            ),
            (
                MarsError::ManifestDeclaredPathMissing {
                    source_name: "origin".to_string(),
                    manifest_path: "./missing".to_string(),
                    package_root: PathBuf::from("/tmp/root"),
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
                MarsError::ModelCacheUnavailable {
                    reason: "MARS_OFFLINE is set and no cached catalog is available".to_string(),
                },
                3,
            ),
            (
                MarsError::Io {
                    operation: "read file".to_string(),
                    path: PathBuf::from("/tmp/file"),
                    source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                },
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
