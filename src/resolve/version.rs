use std::collections::HashMap;

use indexmap::IndexMap;
use semver::{Version, VersionReq};

use crate::diagnostic::DiagnosticCollector;
use crate::error::{MarsError, ResolutionError};
use crate::lock::LockFile;
use crate::source::{AvailableVersion, ResolvedRef};
use crate::types::{SourceName, SourceUrl};

use super::SourceProvider;
use super::package::PendingSource;
use super::types::{ResolveOptions, ResolvedNode, VersionConstraint};

/// Resolve a single source to a concrete version/ref.
pub(crate) fn resolve_single_source(
    pending: &PendingSource,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
    diag: &mut DiagnosticCollector,
) -> Result<(ResolvedRef, Option<Version>), MarsError> {
    match &pending.spec {
        crate::config::SourceSpec::Path(path) => {
            // Path sources: no version resolution, just use the path
            provider
                .fetch_path(path, pending.name.as_ref(), diag)
                .map(|resolved_ref| (resolved_ref, None))
        }
        crate::config::SourceSpec::Git(git) => resolve_git_source(
            &pending.name,
            &git.url,
            constraints
                .get(&pending.name)
                .map(|c| c.as_slice())
                .unwrap_or(&[]),
            provider,
            locked,
            options,
            diag,
        ),
    }
}

/// Resolve a git source: list versions, intersect constraints, select version.
pub(crate) fn resolve_git_source(
    name: &SourceName,
    url: &SourceUrl,
    constraints: &[(String, VersionConstraint)],
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
    diag: &mut DiagnosticCollector,
) -> Result<(ResolvedRef, Option<Version>), MarsError> {
    // If all constraints are ref pins, use the first one
    // (multiple ref pins for the same source is likely an error, but we'll use first)
    let has_ref_pin = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::RefPin(_)));
    if has_ref_pin {
        for (_, constraint) in constraints {
            if let VersionConstraint::RefPin(ref_name) = constraint {
                return provider
                    .fetch_git_ref(url, ref_name, name.as_ref(), None, diag)
                    .map(|resolved_ref| (resolved_ref, None));
            }
        }
    }

    // Check if any constraint is "Latest" — if so, pick newest (not MVS)
    let has_latest = constraints
        .iter()
        .any(|(_, c)| matches!(c, VersionConstraint::Latest));

    let locked_source = locked.and_then(|lf| lf.dependencies.get(name));
    let locked_commit = locked_source.and_then(|ls| ls.commit.as_deref());

    let upgrade_maximize = options.maximize
        && (options.upgrade_targets.is_empty() || options.upgrade_targets.contains(name));

    // Determine whether to maximize this source:
    // - explicit maximize mode (mars upgrade)
    // - "latest" constraint means "newest available"
    let maximize = has_latest || upgrade_maximize;

    // List available versions
    let available = provider.list_versions(url)?;
    let latest = available
        .iter()
        .max_by(|a, b| a.version.cmp(&b.version))
        .map(|v| v.version.clone());

    if available.is_empty() {
        // No semver tags → treat as "latest commit", with locked-commit replay.
        // For untagged sources, replay lock by default unless explicitly upgrading.
        let preferred_commit = if !upgrade_maximize {
            locked_commit
        } else {
            None
        };
        match provider.fetch_git_ref(url, "HEAD", name.as_ref(), preferred_commit, diag) {
            Ok(resolved) => return Ok((resolved, latest)),
            Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => {
                return Err(err);
            }
            Err(MarsError::LockedCommitUnreachable {
                commit,
                url: source_url,
            }) => {
                diag.warn(
                    "locked-commit-unreachable",
                    format!(
                        "locked commit {commit} for {source_url} is unreachable; re-resolving from HEAD"
                    ),
                );
                return provider
                    .fetch_git_ref(url, "HEAD", name.as_ref(), None, diag)
                    .map(|resolved_ref| (resolved_ref, latest));
            }
            Err(err) => return Err(err),
        }
    }

    // Collect all semver constraints
    let semver_reqs: Vec<(&str, &VersionReq)> = constraints
        .iter()
        .filter_map(|(requester, c)| match c {
            VersionConstraint::Semver(req) => Some((requester.as_str(), req)),
            _ => None,
        })
        .collect();

    // Get locked version for this source (if any)
    let locked_version = locked_source
        .and_then(|ls| ls.version.as_ref())
        .and_then(|v| {
            let v = v.strip_prefix('v').unwrap_or(v);
            Version::parse(v).ok()
        });

    // Select version
    let selected = select_version(
        name,
        &available,
        &semver_reqs,
        locked_version.as_ref(),
        maximize,
    )?;

    let should_try_locked_commit = !maximize
        && locked_commit.is_some()
        && match locked_version.as_ref() {
            Some(version) => selected.version == *version,
            None => true,
        };

    let preferred_commit = if should_try_locked_commit {
        locked_commit
    } else {
        None
    };

    match provider.fetch_git_version(url, selected, name.as_ref(), preferred_commit, diag) {
        Ok(resolved) => Ok((resolved, latest)),
        Err(err @ MarsError::LockedCommitUnreachable { .. }) if options.frozen => Err(err),
        Err(MarsError::LockedCommitUnreachable {
            commit,
            url: source_url,
        }) => {
            diag.warn(
                "locked-commit-unreachable",
                format!(
                    "locked commit {commit} for {source_url} is unreachable; re-resolving from tag"
                ),
            );
            provider
                .fetch_git_version(url, selected, name.as_ref(), None, diag)
                .map(|resolved_ref| (resolved_ref, latest))
        }
        Err(err) => Err(err),
    }
}

/// Select a concrete version from available versions, respecting constraints.
///
/// - MVS (default): pick the minimum version satisfying all constraints.
/// - Maximize mode: pick the newest version satisfying all constraints.
/// - Locked version preference: if a locked version satisfies all constraints, use it.
pub(crate) fn select_version<'a>(
    source_name: &SourceName,
    available: &'a [AvailableVersion],
    constraints: &[(&str, &VersionReq)],
    locked: Option<&Version>,
    maximize: bool,
) -> Result<&'a AvailableVersion, MarsError> {
    // Find all versions satisfying all constraints
    let satisfying: Vec<&AvailableVersion> = available
        .iter()
        .filter(|av| {
            if constraints.is_empty() {
                return true;
            }
            constraints.iter().all(|(_, req)| req.matches(&av.version))
        })
        .collect();

    if satisfying.is_empty() {
        // Build helpful error message listing all constraints
        let constraint_desc: Vec<String> = constraints
            .iter()
            .map(|(requester, req)| format!("  `{requester}` requires {req}"))
            .collect();

        let available_desc: Vec<String> =
            available.iter().map(|av| av.version.to_string()).collect();

        return Err(ResolutionError::VersionConflict {
            name: source_name.to_string(),
            message: format!(
                "no version satisfies all constraints:\n{}\navailable versions: [{}]",
                constraint_desc.join("\n"),
                available_desc.join(", ")
            ),
        }
        .into());
    }

    // If we have a locked version and it satisfies constraints, prefer it
    if !maximize
        && let Some(locked_ver) = locked
        && let Some(av) = satisfying.iter().find(|av| av.version == *locked_ver)
    {
        return Ok(av);
    }

    // MVS: pick minimum. Maximize: pick maximum.
    // Available versions from list_versions are sorted ascending by semver.
    if maximize {
        Ok(satisfying.last().expect("satisfying is non-empty"))
    } else {
        Ok(satisfying.first().expect("satisfying is non-empty"))
    }
}

/// Validate that all constraints are satisfied by the resolved versions.
///
/// This catches cases where a source was resolved before all constraints
/// were known (e.g., a later transitive dep adds a new constraint on an
/// already-resolved source).
pub(crate) fn validate_all_constraints(
    nodes: &IndexMap<SourceName, ResolvedNode>,
    constraints: &HashMap<SourceName, Vec<(String, VersionConstraint)>>,
) -> Result<(), MarsError> {
    for (name, constraint_list) in constraints {
        let has_latest = constraint_list
            .iter()
            .any(|(_, constraint)| matches!(constraint, VersionConstraint::Latest));
        let node = match nodes.get(name) {
            Some(n) => n,
            None => continue, // Should not happen, but be safe
        };

        // Only validate semver constraints against resolved versions
        if let Some(ref resolved_ver) = node.resolved_ref.version {
            for (requester, constraint) in constraint_list {
                if has_latest {
                    continue;
                }
                if let VersionConstraint::Semver(req) = constraint
                    && !req.matches(resolved_ver)
                {
                    return Err(ResolutionError::VersionConflict {
                        name: name.to_string(),
                        message: format!(
                            "resolved version {resolved_ver} does not satisfy \
                             constraint {req} (required by `{requester}`)"
                        ),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}
