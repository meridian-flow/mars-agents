//! Git source adapter primitives.

use std::fs;
use std::io::{self, Cursor};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::error::MarsError;
use crate::source::parse::extract_hostname;
use crate::source::{AvailableVersion, GlobalCache, ResolvedRef};
use crate::types::CommitHash;
use flate2::read::GzDecoder;
use tar::Archive;

/// Options controlling git fetch behavior.
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Preferred commit SHA to checkout before resolving tags/versions.
    /// Used for lock replay to guarantee reproducible content.
    pub preferred_commit: Option<CommitHash>,
}

/// Normalize a git URL to a filesystem-safe directory name.
///
/// Strips protocol prefixes and replaces `/` and `:` with `_`.
/// Strips trailing `.git` suffix.
///
/// Examples:
/// - `https://github.com/foo/bar` -> `github.com_foo_bar`
/// - `github.com/foo/bar` -> `github.com_foo_bar`
/// - `git@github.com:foo/bar.git` -> `github.com_foo_bar`
/// - `ssh://git@github.com/foo/bar` -> `github.com_foo_bar`
pub fn url_to_dirname(url: &str) -> String {
    let mut s = url.to_string();

    // Strip common protocol prefixes
    for prefix in &["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }

    // Handle SSH shorthand: git@github.com:foo/bar -> github.com/foo/bar
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
        if let Some(colon_pos) = s.find(':') {
            let after_colon = &s[colon_pos + 1..];
            if !after_colon.starts_with("//") {
                s.replace_range(colon_pos..colon_pos + 1, "/");
            }
        }
    }

    // Strip trailing .git
    if let Some(rest) = s.strip_suffix(".git") {
        s = rest.to_string();
    }

    // Strip trailing slash
    if let Some(rest) = s.strip_suffix('/') {
        s = rest.to_string();
    }

    // Replace `/` with `_`
    s.replace('/', "_")
}

/// Parse a tag name as a semver version tag.
///
/// Accepts: `v1.0.0`, `v0.5.2`, `1.0.0`
/// Rejects: `latest`, `nightly-2024`, or any non-semver tag.
fn parse_semver_tag(tag: &str) -> Option<semver::Version> {
    let version_str = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(version_str).ok()
}

#[derive(Debug, Clone)]
struct ResolvedVersion {
    tag: Option<String>,
    version: Option<semver::Version>,
    sha: String,
}

fn run_command(command: &mut Command, display: String) -> Result<String, MarsError> {
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

fn ls_remote_ref(url: &str, reference: &str) -> Result<String, MarsError> {
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

fn resolve_version(url: &str, version_req: Option<&str>) -> Result<ResolvedVersion, MarsError> {
    if let Some(version_req) = version_req {
        if let Some(requested_version) = parse_semver_tag(version_req) {
            let tags = ls_remote_tags(url)?;
            let selected = tags
                .into_iter()
                .find(|tag| tag.tag == version_req || tag.version == requested_version)
                .ok_or_else(|| MarsError::Source {
                    source_name: url.to_string(),
                    message: format!("version tag `{version_req}` not found"),
                })?;

            return Ok(ResolvedVersion {
                tag: Some(selected.tag),
                version: Some(selected.version),
                sha: selected.commit_id,
            });
        }

        let sha = ls_remote_ref(url, version_req)?;
        return Ok(ResolvedVersion {
            tag: None,
            version: None,
            sha,
        });
    }

    let tags = ls_remote_tags(url)?;
    if let Some(selected) = tags.last() {
        return Ok(ResolvedVersion {
            tag: Some(selected.tag.clone()),
            version: Some(selected.version.clone()),
            sha: selected.commit_id.clone(),
        });
    }

    eprintln!("warning: no releases found for {url}, using latest commit from default branch");
    let sha = ls_remote_head(url)?;
    Ok(ResolvedVersion {
        tag: None,
        version: None,
        sha,
    })
}

fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let (_, tail) = url.split_once("github.com/")?;
    let mut segments = tail.split('/');
    let owner = segments.next()?.trim();
    let repo = segments.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((owner.to_string(), repo.to_string()))
}

fn download_archive_bytes(archive_url: &str) -> Result<Vec<u8>, MarsError> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        match ureq::get(archive_url).call() {
            Ok(mut response) => {
                return response
                    .body_mut()
                    .with_config()
                    .limit(200 * 1024 * 1024)
                    .read_to_vec()
                    .map_err(|err| MarsError::Http {
                        url: archive_url.to_string(),
                        status: 0,
                        message: err.to_string(),
                    });
            }
            Err(ureq::Error::StatusCode(status)) => {
                if status == 429 && attempt < MAX_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(150 * attempt as u64));
                    continue;
                }
                return Err(MarsError::Http {
                    url: archive_url.to_string(),
                    status,
                    message: format!("request failed with HTTP status {status}"),
                });
            }
            Err(err) => {
                return Err(MarsError::Http {
                    url: archive_url.to_string(),
                    status: 0,
                    message: err.to_string(),
                });
            }
        }
    }

    Err(MarsError::Http {
        url: archive_url.to_string(),
        status: 429,
        message: "request failed after retrying HTTP 429".to_string(),
    })
}

fn extract_and_strip_archive(archive_bytes: &[u8], dest: &Path) -> Result<(), MarsError> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();

        if entry_type.is_symlink() || entry_type.is_hard_link() {
            continue;
        }

        let entry_path = entry.path()?;
        if entry_path.is_absolute() {
            return Err(MarsError::InvalidRequest {
                message: format!(
                    "archive entry contains absolute path: {}",
                    entry_path.display()
                ),
            });
        }

        let mut components = entry_path.components();
        // Strip the top-level `{repo}-{sha}/` directory.
        components.next();

        let mut relative_path = PathBuf::new();
        for component in components {
            match component {
                Component::Normal(seg) => relative_path.push(seg),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(MarsError::InvalidRequest {
                        message: format!(
                            "archive entry attempts parent traversal: {}",
                            entry_path.display()
                        ),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(MarsError::InvalidRequest {
                        message: format!(
                            "archive entry has invalid path: {}",
                            entry_path.display()
                        ),
                    });
                }
            }
        }

        if relative_path.as_os_str().is_empty() {
            continue;
        }

        let target_path = dest.join(&relative_path);

        if entry_type.is_dir() {
            fs::create_dir_all(&target_path)?;
            continue;
        }

        if !entry_type.is_file() {
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = fs::File::create(&target_path)?;
        io::copy(&mut entry, &mut output)?;
    }

    Ok(())
}

fn fetch_archive(url: &str, sha: &str, cache: &GlobalCache) -> Result<PathBuf, MarsError> {
    let (owner, repo) = github_owner_repo(url).ok_or_else(|| MarsError::Source {
        source_name: url.to_string(),
        message: "expected GitHub URL in the form https://github.com/owner/repo".to_string(),
    })?;

    let archive_url = format!("https://github.com/{owner}/{repo}/archive/{sha}.tar.gz");
    let cache_path = cache
        .archives_dir()
        .join(format!("{}_{}", url_to_dirname(url), sha));

    if cache_path.exists() {
        return Ok(cache_path);
    }

    let archive_bytes = download_archive_bytes(&archive_url)?;
    let temp_path = PathBuf::from(format!(
        "{}.tmp.{}",
        cache_path.to_string_lossy(),
        std::process::id()
    ));

    if temp_path.exists() {
        let _ = fs::remove_dir_all(&temp_path);
    }
    fs::create_dir_all(&temp_path)?;

    let extract_result = extract_and_strip_archive(&archive_bytes, &temp_path);
    if let Err(err) = extract_result {
        let _ = fs::remove_dir_all(&temp_path);
        return Err(err);
    }

    match fs::rename(&temp_path, &cache_path) {
        Ok(()) => Ok(cache_path),
        Err(err) => {
            // Another process may have won the race and already created the cache path.
            if cache_path.exists() {
                let _ = fs::remove_dir_all(&temp_path);
                Ok(cache_path)
            } else {
                let _ = fs::remove_dir_all(&temp_path);
                Err(err.into())
            }
        }
    }
}

fn fetch_git_clone(
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

/// Return true when the URL host resolves to github.com.
pub fn is_github_host(url: &str) -> bool {
    extract_hostname(url)
        .map(|host| host.eq_ignore_ascii_case("github.com"))
        .unwrap_or(false)
}

fn should_use_github_archive(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.starts_with("git@") || trimmed.starts_with("ssh://") {
        return false;
    }

    trimmed.starts_with("https://") && is_github_host(trimmed)
}

pub fn list_versions(url: &str, _cache: &GlobalCache) -> Result<Vec<AvailableVersion>, MarsError> {
    ls_remote_tags(url)
}

pub fn fetch(
    url: &str,
    version_req: Option<&str>,
    source_name: &str,
    cache: &GlobalCache,
    options: &FetchOptions,
) -> Result<ResolvedRef, MarsError> {
    let mut resolved = resolve_version(url, version_req)?;
    if let Some(preferred_commit) = options.preferred_commit.as_ref() {
        resolved.sha = preferred_commit.to_string();
    }

    let tree_path = if should_use_github_archive(url) {
        match fetch_archive(url, &resolved.sha, cache) {
            Ok(path) => path,
            Err(MarsError::Http { status: 404, .. }) if options.preferred_commit.is_some() => {
                return Err(MarsError::LockedCommitUnreachable {
                    commit: resolved.sha.clone(),
                    url: url.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
    } else {
        // For git clone path, prefer exact SHA checkout when replaying a locked commit,
        // or when resolving branch/default-HEAD refs (non-tag fetches).
        let checkout_sha = if options.preferred_commit.is_some() || resolved.tag.is_none() {
            Some(resolved.sha.as_str())
        } else {
            None
        };

        match fetch_git_clone(url, resolved.tag.as_deref(), checkout_sha, cache) {
            Ok(path) => path,
            Err(MarsError::GitCli { .. }) if options.preferred_commit.is_some() => {
                return Err(MarsError::LockedCommitUnreachable {
                    commit: resolved.sha.clone(),
                    url: url.to_string(),
                });
            }
            Err(err) => return Err(err),
        }
    };

    Ok(ResolvedRef {
        source_name: source_name.into(),
        version: resolved.version,
        version_tag: resolved.tag,
        commit: Some(CommitHash::from(resolved.sha)),
        tree_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use semver::Version;
    use std::ffi::OsStr;
    use std::io::Cursor;
    use std::io::Write;
    use std::path::Path;
    use tar::Builder;
    use tempfile::TempDir;

    fn run_git<I, S>(cwd: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        if !output.status.success() {
            panic!(
                "git command failed: {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let repo = TempDir::new().unwrap();
        run_git(repo.path(), ["init", "."]);
        run_git(repo.path(), ["config", "user.name", "Mars Test"]);
        run_git(repo.path(), ["config", "user.email", "mars@example.com"]);

        fs::write(repo.path().join("README.md"), "initial\n").unwrap();
        run_git(repo.path(), ["add", "."]);
        run_git(repo.path(), ["commit", "-m", "initial commit"]);

        repo
    }

    fn commit_file(repo: &Path, filename: &str, contents: &str, message: &str) -> String {
        fs::write(repo.join(filename), contents).unwrap();
        run_git(repo, ["add", filename]);
        run_git(repo, ["commit", "-m", message]);
        run_git(repo, ["rev-parse", "HEAD"])
    }

    fn build_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);

        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, *path, Cursor::new(*contents))
                .unwrap();
        }

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn build_tar_gz_with_symlink() -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);

        let file_contents = b"safe\n";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_mode(0o644);
        file_header.set_size(file_contents.len() as u64);
        file_header.set_cksum();
        builder
            .append_data(
                &mut file_header,
                "repo-abc/agents/coder.md",
                Cursor::new(file_contents),
            )
            .unwrap();

        let mut symlink_header = tar::Header::new_gnu();
        symlink_header.set_entry_type(tar::EntryType::Symlink);
        symlink_header.set_mode(0o777);
        symlink_header.set_size(0);
        symlink_header.set_cksum();
        builder
            .append_link(&mut symlink_header, "repo-abc/agents/link.md", "coder.md")
            .unwrap();

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn write_tar_field(dst: &mut [u8], value: &[u8]) {
        let len = value.len().min(dst.len());
        dst[..len].copy_from_slice(&value[..len]);
    }

    fn write_tar_octal(dst: &mut [u8], value: u64) {
        let width = dst.len().saturating_sub(1);
        let octal = format!("{value:0width$o}");
        let bytes = octal.as_bytes();
        let copy_len = bytes.len().min(width);
        dst[..copy_len].copy_from_slice(&bytes[..copy_len]);
        dst[dst.len() - 1] = 0;
    }

    fn build_raw_tar_gz_single_file(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        write_tar_field(&mut header[0..100], path.as_bytes());
        write_tar_octal(&mut header[100..108], 0o644);
        write_tar_octal(&mut header[108..116], 0);
        write_tar_octal(&mut header[116..124], 0);
        write_tar_octal(&mut header[124..136], contents.len() as u64);
        write_tar_octal(&mut header[136..148], 0);
        header[156] = b'0';
        write_tar_field(&mut header[257..263], b"ustar\0");
        write_tar_field(&mut header[263..265], b"00");

        for b in &mut header[148..156] {
            *b = b' ';
        }
        let checksum: u32 = header.iter().map(|b| *b as u32).sum();
        let checksum_field = format!("{checksum:06o}\0 ");
        write_tar_field(&mut header[148..156], checksum_field.as_bytes());

        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        let padding = (512 - (contents.len() % 512)) % 512;
        tar.extend(vec![0_u8; padding]);
        tar.extend(vec![0_u8; 1024]);

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    // ==================== url_to_dirname tests ====================

    #[test]
    fn url_to_dirname_https() {
        assert_eq!(
            url_to_dirname("https://github.com/foo/bar"),
            "github.com_foo_bar"
        );
    }

    #[test]
    fn url_to_dirname_bare_domain() {
        assert_eq!(
            url_to_dirname("github.com/haowjy/meridian-base"),
            "github.com_haowjy_meridian-base"
        );
    }

    #[test]
    fn url_to_dirname_ssh() {
        assert_eq!(
            url_to_dirname("git@github.com:foo/bar.git"),
            "github.com_foo_bar"
        );
    }

    #[test]
    fn url_to_dirname_https_with_git_suffix() {
        assert_eq!(
            url_to_dirname("https://github.com/foo/bar.git"),
            "github.com_foo_bar"
        );
    }

    #[test]
    fn url_to_dirname_ssh_protocol() {
        assert_eq!(
            url_to_dirname("ssh://git@github.com/foo/bar"),
            "github.com_foo_bar"
        );
    }

    #[test]
    fn url_to_dirname_http() {
        assert_eq!(
            url_to_dirname("http://gitlab.com/org/repo"),
            "gitlab.com_org_repo"
        );
    }

    #[test]
    fn url_to_dirname_trailing_slash() {
        assert_eq!(
            url_to_dirname("https://github.com/foo/bar/"),
            "github.com_foo_bar"
        );
    }

    // ==================== parse_semver_tag tests ====================

    #[test]
    fn parse_semver_v_prefixed() {
        let v = parse_semver_tag("v1.2.3").unwrap();
        assert_eq!(v, semver::Version::new(1, 2, 3));
    }

    #[test]
    fn parse_semver_no_prefix() {
        let v = parse_semver_tag("0.5.2").unwrap();
        assert_eq!(v, semver::Version::new(0, 5, 2));
    }

    #[test]
    fn parse_semver_prerelease() {
        let v = parse_semver_tag("v2.0.0-rc.1").unwrap();
        assert_eq!(v.major, 2);
        assert!(!v.pre.is_empty());
    }

    #[test]
    fn parse_semver_rejects_non_semver() {
        assert!(parse_semver_tag("latest").is_none());
        assert!(parse_semver_tag("nightly-2024").is_none());
        assert!(parse_semver_tag("release").is_none());
    }

    #[test]
    fn ls_remote_tags_filters_sorts_and_skips_peeled_refs() {
        let repo = init_repo();
        run_git(repo.path(), ["tag", "v1.0.0"]);

        commit_file(repo.path(), "README.md", "second\n", "second commit");
        run_git(repo.path(), ["tag", "-a", "v1.2.0", "-m", "v1.2.0"]);
        run_git(repo.path(), ["tag", "not-a-version"]);

        commit_file(repo.path(), "README.md", "third\n", "third commit");
        run_git(repo.path(), ["tag", "v1.10.0"]);

        let versions = ls_remote_tags(repo.path().to_str().unwrap()).unwrap();
        let tags: Vec<String> = versions.iter().map(|v| v.tag.clone()).collect();
        assert_eq!(tags, vec!["v1.0.0", "v1.2.0", "v1.10.0"]);

        for version in versions {
            assert_eq!(version.commit_id.len(), 40);
            assert!(version.commit_id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn extract_and_strip_archive_flattens_top_level_directory() {
        let tarball = build_tar_gz(&[
            ("repo-abc/agents/coder.md", b"agent"),
            ("repo-abc/skills/review/SKILL.md", b"skill"),
        ]);
        let out = TempDir::new().unwrap();

        extract_and_strip_archive(&tarball, out.path()).unwrap();

        assert_eq!(
            fs::read_to_string(out.path().join("agents/coder.md")).unwrap(),
            "agent"
        );
        assert_eq!(
            fs::read_to_string(out.path().join("skills/review/SKILL.md")).unwrap(),
            "skill"
        );
    }

    #[test]
    fn extract_and_strip_archive_rejects_parent_traversal() {
        let tarball = build_raw_tar_gz_single_file("repo-abc/../escape.txt", b"bad");
        let out = TempDir::new().unwrap();

        let err = extract_and_strip_archive(&tarball, out.path()).unwrap_err();
        assert!(matches!(err, MarsError::InvalidRequest { .. }));
        assert!(!out.path().join("escape.txt").exists());
    }

    #[test]
    fn extract_and_strip_archive_skips_symlinks() {
        let tarball = build_tar_gz_with_symlink();
        let out = TempDir::new().unwrap();

        extract_and_strip_archive(&tarball, out.path()).unwrap();

        assert!(out.path().join("agents/coder.md").exists());
        assert!(!out.path().join("agents/link.md").exists());
    }

    #[test]
    fn fetch_local_git_repo_uses_latest_semver_tag() {
        let remote = init_repo();
        run_git(remote.path(), ["tag", "v0.1.0"]);

        let v020_commit = commit_file(remote.path(), "README.md", "v0.2.0\n", "release v0.2.0");
        run_git(remote.path(), ["tag", "v0.2.0"]);

        let cache_root = TempDir::new().unwrap();
        let cache = GlobalCache {
            root: cache_root.path().join("cache"),
        };
        fs::create_dir_all(cache.archives_dir()).unwrap();
        fs::create_dir_all(cache.git_dir()).unwrap();

        let url = format!("file://{}", remote.path().display());
        let resolved = fetch(&url, None, "local-source", &cache, &FetchOptions::default()).unwrap();

        assert_eq!(resolved.source_name.as_ref(), "local-source");
        assert_eq!(resolved.version, Some(Version::new(0, 2, 0)));
        assert_eq!(resolved.version_tag.as_deref(), Some("v0.2.0"));
        assert_eq!(resolved.commit.as_deref(), Some(v020_commit.as_str()));
        assert!(resolved.tree_path.join("README.md").exists());

        let checked_out = run_git(&resolved.tree_path, ["rev-parse", "HEAD"]);
        assert_eq!(checked_out, v020_commit);
    }

    // ==================== is_github_host tests ====================

    #[test]
    fn is_github_host_accepts_supported_formats() {
        assert!(is_github_host("https://github.com/org/repo"));
        assert!(is_github_host("github.com/org/repo"));
        assert!(is_github_host("git@github.com:org/repo.git"));
        assert!(is_github_host("https://git@github.com:8443/org/repo"));
    }

    #[test]
    fn is_github_host_rejects_other_hosts() {
        assert!(!is_github_host("https://gitlab.com/org/repo"));
        assert!(!is_github_host("git@source.example.com:org/repo.git"));
    }

    #[test]
    fn github_archive_only_for_https_github_urls() {
        assert!(should_use_github_archive("https://github.com/org/repo"));
        assert!(!should_use_github_archive("http://github.com/org/repo"));
        assert!(!should_use_github_archive("github.com/org/repo"));
        assert!(!should_use_github_archive("git@github.com:org/repo.git"));
        assert!(!should_use_github_archive("ssh://git@github.com/org/repo"));
    }
}
