//! `mars outdated` — show available updates without applying.

use std::path::Path;

use serde::Serialize;

use crate::error::MarsError;

use super::output;

/// Arguments for `mars outdated`.
#[derive(Debug, clap::Args)]
pub struct OutdatedArgs {}

/// One row in the outdated report.
#[derive(Debug, Serialize)]
struct OutdatedEntry {
    source: String,
    locked: String,
    constraint: String,
    updateable: String,
    latest: String,
}

/// Run `mars outdated`.
pub fn run(_args: &OutdatedArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(root)?;
    let config = crate::config::load(root)?;
    let cache = crate::source::GlobalCache::new()?;

    let mut entries = Vec::new();

    for (name, source_entry) in &config.sources {
        // Only check git sources with versions
        let url = match &source_entry.url {
            Some(u) => u,
            None => continue, // local path sources have no version
        };

        let locked_version = lock
            .sources
            .get(name)
            .and_then(|s| s.version.clone())
            .unwrap_or_else(|| "-".to_string());

        let constraint = source_entry
            .version
            .clone()
            .unwrap_or_else(|| "latest".to_string());

        // Try to list versions (may fail for non-git sources)
        let versions = match crate::source::list_versions(url, &cache) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if versions.is_empty() {
            // Untagged repo — compare locked commit vs current HEAD
            let current_head = crate::source::git::ls_remote_head(url.as_ref())
                .map(|sha| if sha.len() >= 12 { sha[..12].to_string() } else { sha })
                .unwrap_or_else(|_| "-".to_string());
            let locked_commit = lock
                .sources
                .get(name)
                .and_then(|s| s.commit.as_ref().map(|c| c.to_string()))
                .unwrap_or_else(|| "-".to_string());
            let locked_short = if locked_commit.len() >= 12 {
                locked_commit[..12].to_string()
            } else {
                locked_commit
            };
            entries.push(OutdatedEntry {
                source: name.to_string(),
                locked: locked_short,
                constraint: "HEAD".to_string(),
                updateable: current_head.clone(),
                latest: current_head,
            });
            continue;
        }

        // Find latest version overall
        let latest = versions
            .iter()
            .max_by(|a, b| a.version.cmp(&b.version))
            .map(|v| v.tag.clone())
            .unwrap_or_else(|| "-".to_string());

        // Find latest version matching current constraint
        let parsed_constraint =
            crate::resolve::parse_version_constraint(source_entry.version.as_deref());
        let updateable = match &parsed_constraint {
            crate::resolve::VersionConstraint::Semver(req) => versions
                .iter()
                .filter(|v| req.matches(&v.version))
                .max_by(|a, b| a.version.cmp(&b.version))
                .map(|v| v.tag.clone())
                .unwrap_or_else(|| locked_version.clone()),
            crate::resolve::VersionConstraint::Latest => latest.clone(),
            crate::resolve::VersionConstraint::RefPin(_) => locked_version.clone(),
        };

        entries.push(OutdatedEntry {
            source: name.to_string(),
            locked: locked_version,
            constraint,
            updateable,
            latest,
        });
    }

    if json {
        output::print_json(&entries);
    } else {
        print_outdated_table(&entries);
    }

    Ok(0)
}

fn print_outdated_table(entries: &[OutdatedEntry]) {
    if entries.is_empty() {
        output::print_info("no git sources to check");
        return;
    }

    let name_w = entries
        .iter()
        .map(|e| e.source.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let locked_w = entries
        .iter()
        .map(|e| e.locked.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let constraint_w = entries
        .iter()
        .map(|e| e.constraint.len())
        .max()
        .unwrap_or(10)
        .max(10);
    let update_w = entries
        .iter()
        .map(|e| e.updateable.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "{:<name_w$}  {:<locked_w$}  {:<constraint_w$}  {:<update_w$}  LATEST",
        "SOURCE", "LOCKED", "CONSTRAINT", "UPDATEABLE"
    );

    for entry in entries {
        println!(
            "{:<name_w$}  {:<locked_w$}  {:<constraint_w$}  {:<update_w$}  {}",
            entry.source, entry.locked, entry.constraint, entry.updateable, entry.latest
        );
    }
}
