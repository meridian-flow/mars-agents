//! Git CLI operations — ls-remote, clone, fetch, checkout.

use std::path::PathBuf;
use std::process::Command;

use crate::error::MarsError;
use crate::source::{AvailableVersion, GlobalCache};

use super::git::{parse_semver_tag, url_to_dirname};

pub(crate) fn run_command(command: &mut Command, display: String) -> Result<String, MarsError> {
    let output = command.output().map_err(|err| MarsError::GitCli {
        command: display.clone(),
        message: err.to_string(),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("command exited with status {}", output.status)
        };

        return Err(MarsError::GitCli {
            command: display,
            message,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn ls_remote_ref(url: &str, reference: &str) -> Result<String, MarsError> {
    let mut command = Command::new("git");
    command.arg("ls-remote").arg(url).arg(reference);

    let command_display = format!("git ls-remote {url} {reference}");
    let output = run_command(&mut command, command_display.clone())?;

    for line in output.lines() {
        if let Some((sha, _)) = line.split_once('\t')
            && !sha.trim().is_empty()
        {
            return Ok(sha.trim().to_string());
        }
    }

    Err(MarsError::GitCli {
        command: command_display,
        message: format!("reference `{reference}` not found"),
    })
}

/// Run `git ls-remote --tags <url>` and parse semver tags.
pub fn ls_remote_tags(url: &str) -> Result<Vec<AvailableVersion>, MarsError> {
    let mut command = Command::new("git");
    command.arg("ls-remote").arg("--tags").arg(url);

    let output = run_command(&mut command, format!("git ls-remote --tags {url}"))?;
    let mut versions = Vec::new();

    for line in output.lines() {
        let Some((sha, reference)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = reference.strip_prefix("refs/tags/") else {
            continue;
        };

        // Annotated tags show up twice (`tag` and peeled `tag^{}`).
        // Keep only the non-peeled entry to avoid duplicates.
        if tag.ends_with("^{}") {
            continue;
        }

        let Some(version) = parse_semver_tag(tag) else {
            continue;
        };

        versions.push(AvailableVersion {
            tag: tag.to_string(),
            version,
            commit_id: sha.trim().to_string(),
        });
    }

    versions.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(versions)
}

/// Run `git ls-remote <url> HEAD` and return the default-branch SHA.
pub fn ls_remote_head(url: &str) -> Result<String, MarsError> {
    ls_remote_ref(url, "HEAD")
}

pub(crate) fn fetch_git_clone(
    url: &str,
    tag: Option<&str>,
    sha: Option<&str>,
    cache: &GlobalCache,
) -> Result<PathBuf, MarsError> {
    let cache_path = cache.git_dir().join(url_to_dirname(url));

    // Acquire per-entry lock to prevent cross-repo races on the same cache entry.
    // Held through fetch + checkout, released when _lock drops at function return.
    let lock_path = cache_path.with_extension("lock");
    let _lock = crate::fs::FileLock::acquire(&lock_path)?;

    let cache_path_display = cache_path.to_string_lossy().to_string();
    let was_cached = cache_path.exists();

    if !was_cached {
        let mut command = Command::new("git");
        command.arg("clone").arg("--depth").arg("1");
        if let Some(tag) = tag {
            command.arg("--branch").arg(tag);
        }
        command.arg(url).arg(&cache_path);

        let mut display = String::from("git clone --depth 1");
        if let Some(tag) = tag {
            display.push_str(&format!(" --branch {tag}"));
        }
        display.push_str(&format!(" {url} {cache_path_display}"));

        run_command(&mut command, display)?;
    } else {
        let mut fetch_cmd = Command::new("git");
        fetch_cmd
            .arg("-C")
            .arg(&cache_path)
            .arg("fetch")
            .arg("--depth")
            .arg("1")
            .arg("origin");
        run_command(
            &mut fetch_cmd,
            format!("git -C {cache_path_display} fetch --depth 1 origin"),
        )?;
    }

    if was_cached {
        if let Some(tag) = tag {
            let mut checkout_tag = Command::new("git");
            checkout_tag
                .arg("-C")
                .arg(&cache_path)
                .arg("checkout")
                .arg(tag);
            run_command(
                &mut checkout_tag,
                format!("git -C {cache_path_display} checkout {tag}"),
            )?;
        }

        if let Some(sha) = sha {
            let mut checkout_sha = Command::new("git");
            checkout_sha
                .arg("-C")
                .arg(&cache_path)
                .arg("checkout")
                .arg(sha);
            run_command(
                &mut checkout_sha,
                format!("git -C {cache_path_display} checkout {sha}"),
            )?;
        } else if tag.is_none() {
            let mut checkout_head = Command::new("git");
            checkout_head
                .arg("-C")
                .arg(&cache_path)
                .arg("checkout")
                .arg("origin/HEAD");
            run_command(
                &mut checkout_head,
                format!("git -C {cache_path_display} checkout origin/HEAD"),
            )?;
        }
    } else if let Some(sha) = sha {
        let mut checkout_sha = Command::new("git");
        checkout_sha
            .arg("-C")
            .arg(&cache_path)
            .arg("checkout")
            .arg(sha);
        run_command(
            &mut checkout_sha,
            format!("git -C {cache_path_display} checkout {sha}"),
        )?;
    }

    Ok(cache_path)
}
