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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("git error: {0}")]
    Git(#[from] git2::Error),
}

pub type Result<T> = std::result::Result<T, MarsError>;
