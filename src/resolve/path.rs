use std::path::Path;

use crate::config::SourceSpec;
use crate::error::MarsError;
use crate::types::{SourceId, SourceName, SourceSubpath};

use super::types::RootedSourceRef;

pub(crate) fn apply_subpath(
    source_name: &SourceName,
    checkout_root: &Path,
    subpath: Option<&SourceSubpath>,
) -> Result<RootedSourceRef, MarsError> {
    let package_root = match subpath {
        Some(subpath) => {
            subpath
                .join_under(checkout_root)
                .map_err(|_| MarsError::SubpathTraversal {
                    source_name: source_name.to_string(),
                    subpath: subpath.to_string(),
                    checkout_root: checkout_root.to_path_buf(),
                })?
        }
        None => checkout_root.to_path_buf(),
    };

    if !package_root.exists() {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathMissing {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root does not exist under checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    if !package_root.is_dir() {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathNotDirectory {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root is not a directory under checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    let canonical_checkout = checkout_root
        .canonicalize()
        .map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!(
                "failed to canonicalize checkout root `{}`: {e}",
                checkout_root.display()
            ),
        })?;
    let canonical_package = package_root.canonicalize().map_err(|e| MarsError::Source {
        source_name: source_name.to_string(),
        message: format!(
            "failed to canonicalize package root `{}`: {e}",
            package_root.display()
        ),
    })?;

    if !canonical_package.starts_with(&canonical_checkout) {
        return match subpath {
            Some(subpath) => Err(MarsError::SubpathTraversal {
                source_name: source_name.to_string(),
                subpath: subpath.to_string(),
                checkout_root: checkout_root.to_path_buf(),
            }),
            None => Err(MarsError::Source {
                source_name: source_name.to_string(),
                message: format!(
                    "package root escapes checkout root `{}`",
                    checkout_root.display()
                ),
            }),
        };
    }

    Ok(RootedSourceRef {
        checkout_root: checkout_root.to_path_buf(),
        package_root,
    })
}

pub(crate) fn source_id_for_pending_spec(
    base_root: &Path,
    spec: &SourceSpec,
    subpath: Option<SourceSubpath>,
) -> SourceId {
    match spec {
        SourceSpec::Git(git) => SourceId::git_with_subpath(git.url.clone(), subpath),
        SourceSpec::Path(path) => {
            match SourceId::path_with_subpath(base_root, path, subpath.clone()) {
                Ok(id) => id,
                Err(_) => {
                    let canonical = if path.is_absolute() {
                        path.clone()
                    } else {
                        base_root.join(path)
                    };
                    SourceId::Path { canonical, subpath }
                }
            }
        }
    }
}
