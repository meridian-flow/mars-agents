use std::path::{Path, PathBuf};

use crate::platform::path_syntax::classify_local_source;
use crate::types::{SourceSubpath, SourceUrl};

/// Classification of source input syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    LocalPath,
    GitHubShorthand,
    GitHubAlias,
    GitHubUrl,
    GitLabAlias,
    GitLabUrl,
    GenericGit,
}

/// Structured result of parsing a CLI source specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSourceSpec {
    pub format: SourceFormat,
    pub raw: String,
    pub url: Option<SourceUrl>,
    pub path: Option<PathBuf>,
    pub subpath: Option<SourceSubpath>,
    pub version: Option<String>,
    pub name: String,
}

/// Errors raised while parsing a source specifier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error(
        "cannot determine source type for {input:?} — expected a local path, supported git source, or owner/repo shorthand"
    )]
    UnrecognizedFormat { input: String },

    #[error("unsupported source form for v1: {input:?} ({reason})")]
    UnsupportedSource { input: String, reason: String },

    #[error("SSH URL {input:?} is missing the colon-separated path (expected git@host:owner/repo)")]
    MalformedSshUrl { input: String },

    #[error("cannot derive a name from {input:?}")]
    CannotDeriveName { input: String },

    #[error("URL {input:?} has no repository path component")]
    EmptyUrlPath { input: String },

    #[error("invalid subpath {input:?}: {reason}")]
    InvalidSubpath { input: String, reason: String },

    #[error(
        "tree URL {input:?} uses a slashy branch name that is ambiguous in the path; use the equivalent #ref form instead"
    )]
    SlashyTreeRef { input: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedHttpUrl {
    scheme: String,
    host: String,
    authority: String,
    path_segments: Vec<String>,
}

/// Parse a source specifier into a normalized structured value.
pub fn parse(input: &str) -> Result<ParsedSourceSpec, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::UnrecognizedFormat {
            input: input.to_string(),
        });
    }

    if let Some(path) = classify_local_source(trimmed) {
        let name = derive_path_name(&path, None)?;
        return Ok(ParsedSourceSpec {
            format: SourceFormat::LocalPath,
            raw: input.to_string(),
            url: None,
            path: Some(path),
            subpath: None,
            version: None,
            name,
        });
    }

    let (without_fragment, fragment_version) = split_fragment(trimmed);
    let (base, legacy_version) = if fragment_version.is_none() {
        split_legacy_version(without_fragment)
    } else {
        (without_fragment, None)
    };
    let version = fragment_version.or(legacy_version.map(str::to_string));

    if let Some(spec) = parse_github_alias(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_github_tree_url(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_github_repo_url(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_gitlab_alias(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_gitlab_tree_url(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_gitlab_repo_url(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }
    if let Some(spec) = parse_github_shorthand(base, version.clone())? {
        return Ok(spec.with_raw(input));
    }

    reject_unsupported_url(base)?;

    if let Some(spec) = parse_generic_git(base, version)? {
        return Ok(spec.with_raw(input));
    }

    Err(ParseError::UnrecognizedFormat {
        input: input.to_string(),
    })
}

impl ParsedSourceSpec {
    fn with_raw(mut self, raw: &str) -> Self {
        self.raw = raw.to_string();
        self
    }
}

fn spec_from_git(
    format: SourceFormat,
    repo_url: String,
    repo_name: &str,
    subpath: Option<SourceSubpath>,
    version: Option<String>,
) -> ParsedSourceSpec {
    let name = derive_git_name(repo_name, subpath.as_ref());
    ParsedSourceSpec {
        format,
        raw: String::new(),
        url: Some(SourceUrl::from(repo_url)),
        path: None,
        subpath,
        version,
        name,
    }
}

fn parse_github_alias(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let payload = match input.strip_prefix("github:") {
        Some(payload) => payload,
        None => return Ok(None),
    };

    let segments = collect_non_empty_segments(payload);
    if segments.len() < 2 {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }

    let owner = &segments[0];
    let repo = strip_git_suffix(&segments[1]);
    let subpath = normalize_subpath_segments(&segments[2..])?;
    Ok(Some(spec_from_git(
        SourceFormat::GitHubAlias,
        format!("https://github.com/{owner}/{repo}"),
        repo,
        subpath,
        version,
    )))
}

fn parse_gitlab_alias(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let payload = match input.strip_prefix("gitlab:") {
        Some(payload) => payload,
        None => return Ok(None),
    };

    let segments = collect_non_empty_segments(payload);
    if segments.len() < 2 {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }

    let repo = strip_git_suffix(segments.last().expect("segments checked"));
    Ok(Some(spec_from_git(
        SourceFormat::GitLabAlias,
        format!("https://gitlab.com/{}", segments.join("/")),
        repo,
        None,
        version,
    )))
}

fn parse_github_tree_url(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let url = match parse_http_like_url(input) {
        Some(url) if url.host == "github.com" => url,
        _ => return Ok(None),
    };

    if url.path_segments.len() >= 4 && url.path_segments[2] == "tree" {
        let owner = &url.path_segments[0];
        let repo = strip_git_suffix(&url.path_segments[1]);
        let tree_ref = decode_ref_segment(&url.path_segments[3], input)?;
        let subpath = normalize_subpath_segments(&url.path_segments[4..])?;

        return Ok(Some(spec_from_git(
            SourceFormat::GitHubUrl,
            format!("https://github.com/{owner}/{repo}"),
            repo,
            subpath,
            version.or(Some(tree_ref)),
        )));
    }

    Ok(None)
}

fn parse_github_repo_url(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let url = match parse_http_like_url(input) {
        Some(url) if url.host == "github.com" => url,
        Some(url) if url.host == "github.com" && url.path_segments.is_empty() => {
            return Err(ParseError::EmptyUrlPath {
                input: input.to_string(),
            });
        }
        _ => return Ok(None),
    };

    reject_known_github_downloads(&url, input)?;
    if url.path_segments.len() < 2 {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }
    if url.path_segments.get(2).is_some() {
        return Ok(None);
    }

    let owner = &url.path_segments[0];
    let repo = strip_git_suffix(&url.path_segments[1]);
    Ok(Some(spec_from_git(
        SourceFormat::GitHubUrl,
        format!("https://github.com/{owner}/{repo}"),
        repo,
        None,
        version,
    )))
}

fn parse_gitlab_tree_url(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let url = match parse_http_like_url(input) {
        Some(url) if looks_like_gitlab_host(&url.host) => url,
        _ => return Ok(None),
    };

    let tree_idx = url
        .path_segments
        .windows(2)
        .position(|pair| pair[0] == "-" && pair[1] == "tree");
    let Some(tree_idx) = tree_idx else {
        return Ok(None);
    };

    if tree_idx < 2 || url.path_segments.len() <= tree_idx + 2 {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }

    let repo_path = &url.path_segments[..tree_idx];
    let repo = strip_git_suffix(repo_path.last().expect("repo path checked"));
    let tree_ref = decode_ref_segment(&url.path_segments[tree_idx + 2], input)?;
    let subpath = normalize_subpath_segments(&url.path_segments[(tree_idx + 3)..])?;

    Ok(Some(spec_from_git(
        SourceFormat::GitLabUrl,
        format!("{}://{}/{}", url.scheme, url.authority, repo_path.join("/")),
        repo,
        subpath,
        version.or(Some(tree_ref)),
    )))
}

fn parse_gitlab_repo_url(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    let url = match parse_http_like_url(input) {
        Some(url) if looks_like_gitlab_host(&url.host) => url,
        _ => return Ok(None),
    };

    reject_known_gitlab_downloads(&url, input)?;
    if url.path_segments.len() < 2 {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }
    if url
        .path_segments
        .windows(2)
        .any(|pair| pair[0] == "-" && pair[1] == "tree")
    {
        return Ok(None);
    }
    if url.path_segments.first().is_some_and(|seg| seg == "api") {
        return Err(ParseError::UnsupportedSource {
            input: input.to_string(),
            reason: "GitLab API endpoints are not supported source inputs".to_string(),
        });
    }

    let repo = strip_git_suffix(url.path_segments.last().expect("repo checked"));
    Ok(Some(spec_from_git(
        SourceFormat::GitLabUrl,
        format!(
            "{}://{}/{}",
            url.scheme,
            url.authority,
            url.path_segments.join("/")
        ),
        repo,
        None,
        version,
    )))
}

fn parse_github_shorthand(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    if input.contains(':') || input.contains("://") || input.contains('.') {
        return Ok(None);
    }

    let segments = collect_non_empty_segments(input);
    if segments.len() < 2 {
        return Ok(None);
    }

    let owner = &segments[0];
    let repo = strip_git_suffix(&segments[1]);
    let subpath = normalize_subpath_segments(&segments[2..])?;
    Ok(Some(spec_from_git(
        SourceFormat::GitHubShorthand,
        format!("https://github.com/{owner}/{repo}"),
        repo,
        subpath,
        version,
    )))
}

fn parse_generic_git(
    input: &str,
    version: Option<String>,
) -> Result<Option<ParsedSourceSpec>, ParseError> {
    if is_ssh_shorthand(input) {
        if !input.contains(':') {
            return Err(ParseError::MalformedSshUrl {
                input: input.to_string(),
            });
        }
        let repo = derive_repo_name_from_git(input)?;
        return Ok(Some(spec_from_git(
            SourceFormat::GenericGit,
            input.trim_end_matches('/').to_string(),
            &repo,
            None,
            version,
        )));
    }

    let url = match parse_http_like_url(input) {
        Some(url) => url,
        None => return Ok(None),
    };

    if url.scheme == "ssh" || url.scheme == "git" || input.ends_with(".git") {
        let repo = derive_repo_name_from_segments(&url.path_segments)?;
        let normalized = format!(
            "{}://{}/{}",
            url.scheme,
            url.authority,
            url.path_segments.join("/")
        );
        return Ok(Some(spec_from_git(
            SourceFormat::GenericGit,
            normalized,
            &repo,
            None,
            version,
        )));
    }

    Ok(None)
}

fn reject_unsupported_url(input: &str) -> Result<(), ParseError> {
    let Some(url) = parse_http_like_url(input) else {
        return Ok(());
    };

    if url.path_segments.is_empty() {
        return Err(ParseError::EmptyUrlPath {
            input: input.to_string(),
        });
    }

    let path = url.path_segments.join("/");
    let lower = path.to_ascii_lowercase();

    if lower.ends_with(".zip")
        || lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".gz")
    {
        return Err(ParseError::UnsupportedSource {
            input: input.to_string(),
            reason: "archive-download URLs are not supported in v1".to_string(),
        });
    }

    if lower.ends_with(".md")
        || lower.ends_with(".json")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
    {
        return Err(ParseError::UnsupportedSource {
            input: input.to_string(),
            reason: "direct file-download URLs are not supported in v1".to_string(),
        });
    }

    if url.host != "github.com"
        && !looks_like_gitlab_host(&url.host)
        && !input.ends_with(".git")
        && url.scheme != "ssh"
        && url.scheme != "git"
    {
        return Err(ParseError::UnsupportedSource {
            input: input.to_string(),
            reason: "well-known endpoint URLs are not supported in v1".to_string(),
        });
    }

    Ok(())
}

fn reject_known_github_downloads(url: &ParsedHttpUrl, input: &str) -> Result<(), ParseError> {
    if url.path_segments.len() >= 3 {
        let third = url.path_segments[2].as_str();
        if matches!(third, "releases" | "archive" | "raw" | "blob") {
            return Err(ParseError::UnsupportedSource {
                input: input.to_string(),
                reason: "GitHub download and file URLs are not supported in v1".to_string(),
            });
        }
    }
    Ok(())
}

fn reject_known_gitlab_downloads(url: &ParsedHttpUrl, input: &str) -> Result<(), ParseError> {
    if url
        .path_segments
        .windows(2)
        .any(|pair| pair[0] == "-" && matches!(pair[1].as_str(), "raw" | "archive"))
    {
        return Err(ParseError::UnsupportedSource {
            input: input.to_string(),
            reason: "GitLab download and file URLs are not supported in v1".to_string(),
        });
    }
    Ok(())
}

fn parse_http_like_url(input: &str) -> Option<ParsedHttpUrl> {
    let normalized = if input.starts_with("github.com/") || input.starts_with("gitlab.com/") {
        format!("https://{input}")
    } else {
        input.to_string()
    };

    let (scheme, rest) = normalized.split_once("://")?;
    let authority_and_path = rest.trim_start_matches('/');
    let (authority, path) = authority_and_path
        .split_once('/')
        .unwrap_or((authority_and_path, ""));
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or(authority).to_string();
    let path_segments = collect_non_empty_segments(path);

    Some(ParsedHttpUrl {
        scheme: scheme.to_string(),
        host,
        authority: authority.to_string(),
        path_segments,
    })
}

fn split_fragment(input: &str) -> (&str, Option<String>) {
    match input.rsplit_once('#') {
        Some((base, fragment)) if !fragment.is_empty() => (base, Some(fragment.to_string())),
        _ => (input, None),
    }
}

fn split_legacy_version(input: &str) -> (&str, Option<&str>) {
    let slash_pos = input.rfind('/').unwrap_or(0);
    match input.rsplit_once('@') {
        Some((base, suffix)) if !suffix.is_empty() && input.rfind('@').unwrap_or(0) > slash_pos => {
            (base, Some(suffix))
        }
        _ => (input, None),
    }
}

fn normalize_subpath_segments(segments: &[String]) -> Result<Option<SourceSubpath>, ParseError> {
    if segments.is_empty() {
        return Ok(None);
    }
    let raw = segments.join("/");
    SourceSubpath::new(&raw)
        .map(Some)
        .map_err(|err| ParseError::InvalidSubpath {
            input: raw,
            reason: err.to_string(),
        })
}

fn derive_git_name(repo: &str, subpath: Option<&SourceSubpath>) -> String {
    match subpath {
        Some(subpath) => format!("{repo}/{}", subpath.as_str()),
        None => repo.to_string(),
    }
}

fn derive_path_name(path: &Path, subpath: Option<&SourceSubpath>) -> Result<String, ParseError> {
    // Use Path::file_name() for cross-platform last-component extraction.
    // This handles both `/` and `\` separators correctly on all platforms.
    let base = path
        .file_name()
        .and_then(|f| f.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ParseError::CannotDeriveName {
            input: path.display().to_string(),
        })?;
    Ok(match subpath {
        Some(subpath) => format!("{base}/{}", subpath.as_str()),
        None => base.to_string(),
    })
}

fn derive_repo_name_from_git(input: &str) -> Result<String, ParseError> {
    let (_, repo_path) = input
        .split_once(':')
        .ok_or_else(|| ParseError::MalformedSshUrl {
            input: input.to_string(),
        })?;
    let segments = collect_non_empty_segments(repo_path);
    derive_repo_name_from_segments(&segments)
}

fn derive_repo_name_from_segments(segments: &[String]) -> Result<String, ParseError> {
    segments
        .last()
        .map(|segment| strip_git_suffix(segment).to_string())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| ParseError::CannotDeriveName {
            input: segments.join("/"),
        })
}

fn decode_ref_segment(segment: &str, input: &str) -> Result<String, ParseError> {
    if segment.contains("%2F") || segment.contains("%2f") {
        return Err(ParseError::SlashyTreeRef {
            input: input.to_string(),
        });
    }
    Ok(segment.to_string())
}

fn strip_git_suffix(value: &str) -> &str {
    value.strip_suffix(".git").unwrap_or(value)
}

fn looks_like_gitlab_host(host: &str) -> bool {
    host == "gitlab.com" || host.contains("gitlab")
}

fn collect_non_empty_segments(input: &str) -> Vec<String> {
    input
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_ssh_shorthand(input: &str) -> bool {
    !input.contains("://")
        && input.contains('@')
        && input.contains(':')
        && input.find('@').unwrap_or(usize::MAX) < input.find(':').unwrap_or(0)
}

/// Extract hostname from a URL-like git source string.
pub fn extract_hostname(input: &str) -> Option<String> {
    if is_ssh_shorthand(input) {
        let (user_host, path) = input.split_once(':')?;
        if path.trim_matches('/').is_empty() {
            return None;
        }
        return user_host.split_once('@').map(|(_, host)| host.to_string());
    }

    parse_http_like_url(input).map(|url| url.host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_local_path_wins_before_shorthand() {
        let parsed = parse("../repo").unwrap();
        assert_eq!(parsed.format, SourceFormat::LocalPath);
        assert_eq!(parsed.path.as_deref(), Some(Path::new("../repo")));
        assert!(parsed.url.is_none());
    }

    #[test]
    fn parse_windows_backslash_source_as_local_path() {
        let parsed = parse("packages\\agents").unwrap();
        assert_eq!(parsed.format, SourceFormat::LocalPath);
        assert_eq!(parsed.path.as_deref(), Some(Path::new("packages\\agents")));
        assert!(parsed.url.is_none());
        assert_eq!(parsed.name, "packages\\agents");
    }

    #[test]
    fn parse_windows_drive_relative_source_as_local_path() {
        let parsed = parse("C:agents").unwrap();
        assert_eq!(parsed.format, SourceFormat::LocalPath);
        assert_eq!(parsed.path.as_deref(), Some(Path::new("C:agents")));
        assert!(parsed.url.is_none());
        assert_eq!(parsed.name, "C:agents");
    }

    #[test]
    fn parse_windows_extended_path_remains_unsupported() {
        let err = parse("\\\\?\\C:\\agents").unwrap_err();
        assert!(matches!(err, ParseError::UnrecognizedFormat { .. }));
    }

    #[test]
    fn parse_github_shorthand_with_subpath() {
        let parsed = parse("owner/repo/plugins/foo").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitHubShorthand);
        assert_eq!(parsed.url.as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(
            parsed.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
        assert_eq!(parsed.name, "repo/plugins/foo");
    }

    #[test]
    fn parse_github_alias_with_subpath() {
        let parsed = parse("github:owner/repo/plugins/foo").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitHubAlias);
        assert_eq!(parsed.url.as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(
            parsed.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    #[test]
    fn parse_github_tree_url_with_ref_and_subpath() {
        let parsed = parse("https://github.com/owner/repo/tree/main/plugins/foo").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitHubUrl);
        assert_eq!(parsed.url.as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(parsed.version.as_deref(), Some("main"));
        assert_eq!(
            parsed.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    #[test]
    fn parse_gitlab_alias_preserves_repo_coordinate_only() {
        let parsed = parse("gitlab:group/subgroup/repo").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitLabAlias);
        assert_eq!(
            parsed.url.as_deref(),
            Some("https://gitlab.com/group/subgroup/repo")
        );
        assert!(parsed.subpath.is_none());
        assert_eq!(parsed.name, "repo");
    }

    #[test]
    fn parse_gitlab_tree_url_custom_host() {
        let parsed =
            parse("https://gitlab.example.com/group/subgroup/repo/-/tree/main/plugins/foo")
                .unwrap();
        assert_eq!(parsed.format, SourceFormat::GitLabUrl);
        assert_eq!(
            parsed.url.as_deref(),
            Some("https://gitlab.example.com/group/subgroup/repo")
        );
        assert_eq!(parsed.version.as_deref(), Some("main"));
        assert_eq!(
            parsed.subpath.as_ref().map(SourceSubpath::as_str),
            Some("plugins/foo")
        );
    }

    #[test]
    fn parse_gitlab_plain_repo_url_custom_host() {
        let parsed = parse("https://gitlab.example.com/group/subgroup/repo").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitLabUrl);
        assert_eq!(
            parsed.url.as_deref(),
            Some("https://gitlab.example.com/group/subgroup/repo")
        );
        assert!(parsed.subpath.is_none());
        assert_eq!(parsed.name, "repo");
    }

    #[test]
    fn parse_gitlab_repo_url_preserves_explicit_port() {
        let parsed = parse("git://gitlab.localtest.me:19424/group/pkg.git").unwrap();
        assert_eq!(parsed.format, SourceFormat::GitLabUrl);
        assert_eq!(
            parsed.url.as_deref(),
            Some("git://gitlab.localtest.me:19424/group/pkg.git")
        );
        assert!(parsed.subpath.is_none());
        assert_eq!(parsed.name, "pkg");
    }

    #[test]
    fn parse_generic_git_ssh_source() {
        let parsed = parse("git@example.com:org/repo.git").unwrap();
        assert_eq!(parsed.format, SourceFormat::GenericGit);
        assert_eq!(parsed.url.as_deref(), Some("git@example.com:org/repo.git"));
        assert!(parsed.subpath.is_none());
        assert_eq!(parsed.name, "repo");
    }

    #[test]
    fn parse_generic_git_preserves_explicit_port() {
        let parsed = parse("git://127.0.0.1:19421/group/pkg.git").unwrap();
        assert_eq!(parsed.format, SourceFormat::GenericGit);
        assert_eq!(
            parsed.url.as_deref(),
            Some("git://127.0.0.1:19421/group/pkg.git")
        );
        assert!(parsed.subpath.is_none());
        assert_eq!(parsed.name, "pkg");
    }

    #[test]
    fn parse_fragment_ref_beats_legacy_at_version() {
        let parsed = parse("owner/repo#feature/x").unwrap();
        assert_eq!(parsed.version.as_deref(), Some("feature/x"));
        assert_eq!(parsed.url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn parse_legacy_at_version_still_supported() {
        let parsed = parse("owner/repo@v1.2.3").unwrap();
        assert_eq!(parsed.version.as_deref(), Some("v1.2.3"));
        assert_eq!(parsed.url.as_deref(), Some("https://github.com/owner/repo"));
    }

    #[test]
    fn rejects_archive_download_url() {
        let err = parse("https://github.com/owner/repo/archive/refs/heads/main.zip").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedSource { .. }));
    }

    #[test]
    fn rejects_file_download_url() {
        let err = parse("https://raw.githubusercontent.com/owner/repo/main/SKILL.md").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedSource { .. }));
    }

    #[test]
    fn rejects_slashy_tree_ref_when_encoded() {
        let err = parse("https://github.com/owner/repo/tree/feature%2Fx/plugins/foo").unwrap_err();
        assert!(matches!(err, ParseError::SlashyTreeRef { .. }));
    }

    #[test]
    fn extract_hostname_supports_ssh_and_https() {
        assert_eq!(
            extract_hostname("git@example.com:org/repo.git").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            extract_hostname("https://github.com/owner/repo").as_deref(),
            Some("github.com")
        );
    }
}
