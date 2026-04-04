//! Git source adapter — strategy and public API.
//!
//! Delegates to `git_cli` for git CLI operations and `archive` for
//! GitHub archive download/extraction.

use crate::error::MarsError;
use crate::source::parse::extract_hostname;
use crate::source::{AvailableVersion, GlobalCache, ResolvedRef};
use crate::types::CommitHash;

use super::archive;
use super::git_cli;

// Re-export for backward compatibility
pub use git_cli::{ls_remote_head, ls_remote_tags};

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
pub(crate) fn parse_semver_tag(tag: &str) -> Option<semver::Version> {
    let version_str = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(version_str).ok()
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedVersion {
    pub tag: Option<String>,
    pub version: Option<semver::Version>,
    pub sha: String,
}

fn resolve_version(url: &str, version_req: Option<&str>) -> Result<ResolvedVersion, MarsError> {
    if let Some(version_req) = version_req {
        if let Some(requested_version) = parse_semver_tag(version_req) {
            let tags = git_cli::ls_remote_tags(url)?;
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

        let sha = git_cli::ls_remote_ref(url, version_req)?;
        return Ok(ResolvedVersion {
            tag: None,
            version: None,
            sha,
        });
    }

    let tags = git_cli::ls_remote_tags(url)?;
    if let Some(selected) = tags.last() {
        return Ok(ResolvedVersion {
            tag: Some(selected.tag.clone()),
            version: Some(selected.version.clone()),
            sha: selected.commit_id.clone(),
        });
    }

    eprintln!("warning: no releases found for {url}, using latest commit from default branch");
    let sha = git_cli::ls_remote_head(url)?;
    Ok(ResolvedVersion {
        tag: None,
        version: None,
        sha,
    })
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
    git_cli::ls_remote_tags(url)
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
        match archive::fetch_archive(url, &resolved.sha, cache) {
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

        match git_cli::fetch_git_clone(url, resolved.tag.as_deref(), checkout_sha, cache) {
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
    use semver::Version;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
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
