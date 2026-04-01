//! Git source adapter using git2 exclusively (no subprocess).
//!
//! Handles cloning, fetching, tag listing, and cache management for git sources.
//! Cache layout: `{cache_dir}/{url_to_dirname}/`

use std::path::Path;

use crate::error::MarsError;
use crate::source::{AvailableVersion, ResolvedRef};

/// Return type for checkout helpers: (semver version, tag name, commit SHA).
type CheckoutResult = (Option<semver::Version>, Option<String>, Option<String>);

/// Normalize a git URL to a filesystem-safe directory name.
///
/// Strips protocol prefixes and replaces `/` and `:` with `_`.
/// Strips trailing `.git` suffix.
///
/// Examples:
/// - `https://github.com/foo/bar` → `github.com_foo_bar`
/// - `github.com/foo/bar` → `github.com_foo_bar`
/// - `git@github.com:foo/bar.git` → `github.com_foo_bar`
/// - `ssh://git@github.com/foo/bar` → `github.com_foo_bar`
pub fn url_to_dirname(url: &str) -> String {
    let mut s = url.to_string();

    // Strip common protocol prefixes
    for prefix in &["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }

    // Handle SSH shorthand: git@github.com:foo/bar → github.com/foo/bar
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
        // Replace the first `:` (host:path separator) with `/`
        if let Some(colon_pos) = s.find(':') {
            // Only replace if it looks like host:path (not host:port/path)
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

/// List available versions from a git remote by reading tags.
///
/// Uses `git2` ls-remote equivalent. Parses tags matching `v{semver}` pattern.
/// Returns sorted by semver version (ascending).
pub fn list_versions(url: &str, cache_dir: &Path) -> Result<Vec<AvailableVersion>, MarsError> {
    // Try to use an existing cached repo to list remote refs (avoids a full clone)
    let versions = list_versions_via_ls_remote(url, cache_dir)?;
    let mut versions = versions;
    versions.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(versions)
}

/// List versions by connecting to the remote (ls-remote style).
fn list_versions_via_ls_remote(
    url: &str,
    cache_dir: &Path,
) -> Result<Vec<AvailableVersion>, MarsError> {
    // If we have a cached repo, open it and use its remote.
    // Otherwise, try create_detached remote.
    let dirname = url_to_dirname(url);
    let cache_path = cache_dir.join(&dirname);

    let versions = if cache_path.exists() {
        let repo = git2::Repository::open(&cache_path).map_err(|e| MarsError::Source {
            source_name: url.to_string(),
            message: format!("failed to open cached repo: {e}"),
        })?;
        list_tags_from_repo(&repo)?
    } else {
        list_tags_via_detached_remote(url)?
    };

    Ok(versions)
}

/// List tags from an already-cloned repository.
fn list_tags_from_repo(repo: &git2::Repository) -> Result<Vec<AvailableVersion>, MarsError> {
    let mut versions = Vec::new();
    let tags = repo.tag_names(None).map_err(|e| MarsError::Source {
        source_name: String::new(),
        message: format!("failed to list tags: {e}"),
    })?;

    for tag_name in tags.iter().flatten() {
        if let Some(av) = parse_version_tag(tag_name, repo) {
            versions.push(av);
        }
    }

    Ok(versions)
}

/// List tags via detached remote (no local clone needed).
fn list_tags_via_detached_remote(url: &str) -> Result<Vec<AvailableVersion>, MarsError> {
    let mut remote =
        git2::Remote::create_detached(url).map_err(|e| wrap_git_error(url, &e))?;

    // Connect to read remote refs
    remote
        .connect(git2::Direction::Fetch)
        .map_err(|e| wrap_git_error(url, &e))?;

    let mut versions = Vec::new();
    let refs = remote.list().map_err(|e| wrap_git_error(url, &e))?;

    for head in refs {
        let refname = head.name();
        // Tags are refs/tags/<name>; skip peeled refs (^{})
        if let Some(tag_name) = refname.strip_prefix("refs/tags/") {
            if tag_name.ends_with("^{}") {
                continue;
            }
            if let Some(version) = parse_semver_tag(tag_name) {
                versions.push(AvailableVersion {
                    tag: tag_name.to_string(),
                    version,
                    commit_id: head.oid(),
                });
            }
        }
    }

    remote.disconnect().ok();
    Ok(versions)
}

/// Parse a tag name as a semver version tag.
///
/// Accepts: `v1.0.0`, `v0.5.2`, `1.0.0`
/// Rejects: `latest`, `nightly-2024`, or any non-semver tag.
fn parse_semver_tag(tag: &str) -> Option<semver::Version> {
    let version_str = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(version_str).ok()
}

/// Parse a version tag from a local repo, resolving the tag to a commit OID.
fn parse_version_tag(tag_name: &str, repo: &git2::Repository) -> Option<AvailableVersion> {
    let version = parse_semver_tag(tag_name)?;

    // Resolve tag to commit OID
    let refname = format!("refs/tags/{tag_name}");
    let reference = repo.find_reference(&refname).ok()?;
    let oid = reference.peel_to_commit().ok()?.id();

    Some(AvailableVersion {
        tag: tag_name.to_string(),
        version,
        commit_id: oid,
    })
}

/// Fetch a git source: clone or update cache, checkout target ref.
///
/// If `version_req` is provided, finds the best matching tag.
/// Otherwise fetches the default branch.
pub fn fetch(
    url: &str,
    version_req: Option<&str>,
    source_name: &str,
    cache_dir: &Path,
) -> Result<ResolvedRef, MarsError> {
    let dirname = url_to_dirname(url);
    let cache_path = cache_dir.join(&dirname);

    let repo = if cache_path.exists() {
        // Update existing cached repo
        update_cached_repo(&cache_path, url, source_name)?
    } else {
        // Clone fresh
        clone_repo(url, &cache_path, source_name)?
    };

    // Determine what to checkout
    let (version, version_tag, commit) = if let Some(req) = version_req {
        checkout_version(&repo, req, source_name)?
    } else {
        // Default branch — HEAD
        checkout_head(&repo, source_name)?
    };

    Ok(ResolvedRef {
        source_name: source_name.to_string(),
        version,
        version_tag,
        commit,
        tree_path: cache_path,
    })
}

/// Clone a repository into the cache directory.
fn clone_repo(url: &str, dest: &Path, source_name: &str) -> Result<git2::Repository, MarsError> {
    // Ensure parent dir exists
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let repo = git2::Repository::clone(url, dest).map_err(|e| wrap_git_error_named(source_name, url, &e))?;
    Ok(repo)
}

/// Update an existing cached repository by fetching from origin.
fn update_cached_repo(
    cache_path: &Path,
    url: &str,
    source_name: &str,
) -> Result<git2::Repository, MarsError> {
    let repo =
        git2::Repository::open(cache_path).map_err(|e| wrap_git_error_named(source_name, url, &e))?;

    // Fetch all refs from origin (including tags)
    {
        let mut remote = repo
            .find_remote("origin")
            .or_else(|_| repo.remote_anonymous(url))
            .map_err(|e| wrap_git_error_named(source_name, url, &e))?;

        let refspecs: &[&str] = &[
            "+refs/heads/*:refs/remotes/origin/*",
            "+refs/tags/*:refs/tags/*",
        ];
        remote
            .fetch(refspecs, None, None)
            .map_err(|e| wrap_git_error_named(source_name, url, &e))?;
    }

    Ok(repo)
}

/// Checkout a specific version (tag) in the repo.
///
/// Finds the best matching tag for the version string.
/// The `version_req` can be a tag name like "v1.0.0" or a bare version "1.0.0".
fn checkout_version(
    repo: &git2::Repository,
    version_req: &str,
    source_name: &str,
) -> Result<CheckoutResult, MarsError> {
    // Try exact tag match first: "v1.0.0" or the version_req directly
    let tag_candidates = [
        version_req.to_string(),
        format!("v{version_req}"),
    ];

    for tag_name in &tag_candidates {
        let refname = format!("refs/tags/{tag_name}");
        if let Ok(reference) = repo.find_reference(&refname) {
            let obj = reference.peel(git2::ObjectType::Commit).map_err(|e| {
                MarsError::Source {
                    source_name: source_name.to_string(),
                    message: format!("failed to peel tag `{tag_name}`: {e}"),
                }
            })?;
            let commit_id = obj.id().to_string();

            // Detach HEAD at the commit
            repo.set_head_detached(obj.id()).map_err(|e| MarsError::Source {
                source_name: source_name.to_string(),
                message: format!("failed to checkout tag `{tag_name}`: {e}"),
            })?;
            repo.checkout_head(Some(
                git2::build::CheckoutBuilder::new().force(),
            ))
            .map_err(|e| MarsError::Source {
                source_name: source_name.to_string(),
                message: format!("failed to checkout working tree for `{tag_name}`: {e}"),
            })?;

            let version = parse_semver_tag(tag_name);
            return Ok((version, Some(tag_name.clone()), Some(commit_id)));
        }
    }

    // No exact tag match — try as a commit SHA
    if let Ok(oid) = git2::Oid::from_str(version_req)
        && let Ok(commit) = repo.find_commit(oid)
    {
        repo.set_head_detached(commit.id()).map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("failed to checkout commit `{version_req}`: {e}"),
        })?;
        repo.checkout_head(Some(
            git2::build::CheckoutBuilder::new().force(),
        ))
        .map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("failed to checkout working tree for commit `{version_req}`: {e}"),
        })?;
        return Ok((None, None, Some(oid.to_string())));
    }

    Err(MarsError::Source {
        source_name: source_name.to_string(),
        message: format!(
            "version `{version_req}` not found — no matching tag or commit"
        ),
    })
}

/// Checkout HEAD (default branch).
fn checkout_head(
    repo: &git2::Repository,
    source_name: &str,
) -> Result<CheckoutResult, MarsError> {
    let head = repo.head().map_err(|e| MarsError::Source {
        source_name: source_name.to_string(),
        message: format!("failed to read HEAD: {e}"),
    })?;

    let commit_id = head
        .peel_to_commit()
        .map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("failed to resolve HEAD to commit: {e}"),
        })?
        .id()
        .to_string();

    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("failed to checkout HEAD: {e}"),
        })?;

    Ok((None, None, Some(commit_id)))
}

/// Wrap a git2 error with source URL context, providing helpful messages
/// for common failure modes (auth, network).
fn wrap_git_error(url: &str, err: &git2::Error) -> MarsError {
    wrap_git_error_named(url, url, err)
}

/// Wrap a git2 error with both source name and URL context.
fn wrap_git_error_named(source_name: &str, url: &str, err: &git2::Error) -> MarsError {
    let message = match err.class() {
        git2::ErrorClass::Ssh | git2::ErrorClass::Http => {
            format!(
                "authentication/network error fetching `{url}`: {err} — \
                 check SSH keys or access token configuration"
            )
        }
        git2::ErrorClass::Net => {
            format!("network error fetching `{url}`: {err}")
        }
        _ => {
            format!("git error for `{url}`: {err}")
        }
    };
    MarsError::Source {
        source_name: source_name.to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a bare git repo with tagged commits.
    fn create_test_repo(dir: &Path) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();

        // Configure author for commits
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();

        repo
    }

    /// Helper: create a commit with a file in it.
    fn create_commit_with_file(
        repo: &git2::Repository,
        filename: &str,
        content: &str,
        message: &str,
    ) -> git2::Oid {
        // Write file to working dir
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(filename), content).unwrap();

        // Add to index
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(filename)).unwrap();
        index.write().unwrap();

        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();

        let sig = repo.signature().unwrap();
        let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .unwrap()
    }

    /// Helper: create a lightweight tag pointing at a commit.
    fn create_tag(repo: &git2::Repository, tag_name: &str, target: git2::Oid) {
        let obj = repo.find_object(target, None).unwrap();
        repo.tag_lightweight(tag_name, &obj, false).unwrap();
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

    // ==================== list_versions tests (with real repos) ====================

    #[test]
    fn list_versions_from_local_repo() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);

        // Create commits and tags
        let c1 = create_commit_with_file(&repo, "file.txt", "v1", "first");
        create_tag(&repo, "v1.0.0", c1);

        let c2 = create_commit_with_file(&repo, "file.txt", "v2", "second");
        create_tag(&repo, "v2.0.0", c2);

        let c3 = create_commit_with_file(&repo, "file.txt", "v3", "third");
        create_tag(&repo, "v0.5.2", c3);

        // Also add a non-semver tag
        create_tag(&repo, "latest", c3);

        // Clone into cache to simulate a cached repo
        let dirname = url_to_dirname(&repo_dir.to_string_lossy());
        let cache_repo_path = cache_dir.join(&dirname);
        git2::Repository::clone(repo_dir.to_str().unwrap(), &cache_repo_path).unwrap();

        // list_versions should find the semver tags
        let versions = list_versions(repo_dir.to_str().unwrap(), &cache_dir).unwrap();

        // Should be sorted ascending
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].version, semver::Version::new(0, 5, 2));
        assert_eq!(versions[0].tag, "v0.5.2");
        assert_eq!(versions[1].version, semver::Version::new(1, 0, 0));
        assert_eq!(versions[1].tag, "v1.0.0");
        assert_eq!(versions[2].version, semver::Version::new(2, 0, 0));
        assert_eq!(versions[2].tag, "v2.0.0");
    }

    #[test]
    fn list_versions_no_semver_tags_returns_empty() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        let c1 = create_commit_with_file(&repo, "file.txt", "data", "init");
        create_tag(&repo, "latest", c1);
        create_tag(&repo, "nightly", c1);

        // Clone into cache
        let dirname = url_to_dirname(&repo_dir.to_string_lossy());
        let cache_repo_path = cache_dir.join(&dirname);
        git2::Repository::clone(repo_dir.to_str().unwrap(), &cache_repo_path).unwrap();

        let versions = list_versions(repo_dir.to_str().unwrap(), &cache_dir).unwrap();
        assert!(versions.is_empty());
    }

    // ==================== fetch tests ====================

    #[test]
    fn fetch_clones_to_cache() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        let c1 = create_commit_with_file(&repo, "hello.txt", "hello world", "init");
        create_tag(&repo, "v1.0.0", c1);

        let resolved = fetch(
            repo_dir.to_str().unwrap(),
            Some("v1.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        assert_eq!(resolved.source_name, "test-source");
        assert_eq!(resolved.version, Some(semver::Version::new(1, 0, 0)));
        assert_eq!(resolved.version_tag.as_deref(), Some("v1.0.0"));
        assert!(resolved.commit.is_some());
        assert!(resolved.tree_path.exists());

        // Verify the file was checked out
        let content = fs::read_to_string(resolved.tree_path.join("hello.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn fetch_reuses_cache_on_second_call() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        let c1 = create_commit_with_file(&repo, "file.txt", "v1", "init");
        create_tag(&repo, "v1.0.0", c1);

        // First fetch — clones
        let r1 = fetch(
            repo_dir.to_str().unwrap(),
            Some("v1.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        // Second fetch — should reuse cache (same path)
        let r2 = fetch(
            repo_dir.to_str().unwrap(),
            Some("v1.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        assert_eq!(r1.tree_path, r2.tree_path);
        assert_eq!(r1.commit, r2.commit);
    }

    #[test]
    fn fetch_picks_up_new_tags_on_update() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        let c1 = create_commit_with_file(&repo, "file.txt", "v1", "init");
        create_tag(&repo, "v1.0.0", c1);

        // First fetch clones
        fetch(
            repo_dir.to_str().unwrap(),
            Some("v1.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        // Add new tag to origin
        let c2 = create_commit_with_file(&repo, "file.txt", "v2", "update");
        create_tag(&repo, "v2.0.0", c2);

        // Second fetch should pick up v2.0.0
        let r2 = fetch(
            repo_dir.to_str().unwrap(),
            Some("v2.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        assert_eq!(r2.version, Some(semver::Version::new(2, 0, 0)));
        assert_eq!(r2.version_tag.as_deref(), Some("v2.0.0"));
    }

    #[test]
    fn fetch_default_branch_without_version() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        create_commit_with_file(&repo, "file.txt", "content", "init");

        let resolved = fetch(
            repo_dir.to_str().unwrap(),
            None,
            "test-source",
            &cache_dir,
        )
        .unwrap();

        assert!(resolved.version.is_none());
        assert!(resolved.version_tag.is_none());
        assert!(resolved.commit.is_some());
        assert!(resolved.tree_path.exists());
    }

    #[test]
    fn fetch_version_not_found_returns_error() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        create_commit_with_file(&repo, "file.txt", "content", "init");

        let result = fetch(
            repo_dir.to_str().unwrap(),
            Some("v99.0.0"),
            "test-source",
            &cache_dir,
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("v99.0.0"), "error should mention the version: {err}");
        assert!(err.contains("test-source"), "error should mention source name: {err}");
    }

    #[test]
    fn fetch_bare_version_finds_v_prefixed_tag() {
        let dir = TempDir::new().unwrap();
        let repo_dir = dir.path().join("origin");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let repo = create_test_repo(&repo_dir);
        let c1 = create_commit_with_file(&repo, "file.txt", "data", "init");
        create_tag(&repo, "v1.0.0", c1);

        // Request "1.0.0" (no v prefix) — should find tag "v1.0.0"
        let resolved = fetch(
            repo_dir.to_str().unwrap(),
            Some("1.0.0"),
            "test-source",
            &cache_dir,
        )
        .unwrap();

        assert_eq!(resolved.version, Some(semver::Version::new(1, 0, 0)));
        assert_eq!(resolved.version_tag.as_deref(), Some("v1.0.0"));
    }
}
