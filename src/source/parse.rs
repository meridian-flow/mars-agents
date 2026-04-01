use std::path::PathBuf;

use crate::types::SourceUrl;

/// Classification of source input syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    LocalPath,
    GitHubShorthand,
    HttpsUrl,
    SshUrl,
    BareDomain,
    Unknown,
}

/// Structured result of parsing a CLI source specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSourceSpec {
    pub format: SourceFormat,
    pub raw: String,
    pub url: Option<SourceUrl>,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub name: String,
}

/// Errors raised while parsing a source specifier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error(
        "cannot determine source type for {input:?} — expected a path, URL, or owner/repo shorthand"
    )]
    UnrecognizedFormat { input: String },

    #[error("SSH URL {input:?} is missing the colon-separated path (expected git@host:owner/repo)")]
    MalformedSshUrl { input: String },

    #[error("cannot derive a name from {input:?}")]
    CannotDeriveName { input: String },

    #[error("URL {input:?} has no path component")]
    EmptyUrlPath { input: String },
}

/// Normalized source kind produced by `normalize()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedSource {
    Git(SourceUrl),
    Path(PathBuf),
}

/// Classify source input without mutating it.
pub fn classify(input: &str) -> SourceFormat {
    if input.starts_with('.') || input.starts_with('/') || input.starts_with('~') {
        return SourceFormat::LocalPath;
    }

    if input.starts_with("https://") || input.starts_with("http://") {
        return SourceFormat::HttpsUrl;
    }

    if !input.contains("://")
        && let Some(at_pos) = input.find('@')
        && let Some(colon_rel) = input[at_pos + 1..].find(':')
    {
        let colon_abs = at_pos + 1 + colon_rel;
        if colon_abs + 1 < input.len() {
            return SourceFormat::SshUrl;
        }
    }

    let shorthand_base = strip_suffix_at(input);
    let slash_count = shorthand_base.chars().filter(|&c| c == '/').count();
    if slash_count == 1 && !shorthand_base.contains(':') {
        let mut segments = shorthand_base.split('/');
        let owner = segments.next().unwrap_or_default();
        let repo = segments.next().unwrap_or_default();
        if !owner.is_empty() && !repo.is_empty() && !owner.contains('.') {
            return SourceFormat::GitHubShorthand;
        }
    }

    if input.contains('.') && input.contains('/') && !input.contains(':') {
        return SourceFormat::BareDomain;
    }

    SourceFormat::Unknown
}

/// Split optional `@version` suffix.
///
/// Version suffix extraction is format-aware:
/// - Enabled for shorthand and HTTPS/bare-domain URLs.
/// - Disabled for SSH URLs and local paths.
pub fn split_version(input: &str, format: SourceFormat) -> (&str, Option<&str>) {
    match format {
        SourceFormat::LocalPath | SourceFormat::SshUrl | SourceFormat::Unknown => (input, None),
        SourceFormat::GitHubShorthand | SourceFormat::HttpsUrl | SourceFormat::BareDomain => {
            if let Some((base, suffix)) = input.rsplit_once('@') {
                if suffix.is_empty() {
                    return (input, None);
                }
                (base, Some(suffix))
            } else {
                (input, None)
            }
        }
    }
}

/// Normalize input into canonical URL/path form.
pub fn normalize(input: &str, format: SourceFormat) -> Result<NormalizedSource, ParseError> {
    match format {
        SourceFormat::LocalPath => Ok(NormalizedSource::Path(PathBuf::from(input))),
        SourceFormat::GitHubShorthand => Ok(NormalizedSource::Git(SourceUrl::from(format!(
            "github.com/{input}"
        )))),
        SourceFormat::HttpsUrl => {
            let stripped = input
                .strip_prefix("https://")
                .or_else(|| input.strip_prefix("http://"))
                .unwrap_or(input);
            let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
            let stripped = stripped.trim_end_matches('/');
            if stripped.is_empty() || !stripped.contains('/') {
                return Err(ParseError::EmptyUrlPath {
                    input: input.to_string(),
                });
            }
            Ok(NormalizedSource::Git(SourceUrl::from(stripped.to_string())))
        }
        SourceFormat::SshUrl => {
            let (user_host, path) =
                input
                    .split_once(':')
                    .ok_or_else(|| ParseError::MalformedSshUrl {
                        input: input.to_string(),
                    })?;
            let host = user_host
                .split_once('@')
                .map(|(_, host)| host)
                .ok_or_else(|| ParseError::MalformedSshUrl {
                    input: input.to_string(),
                })?;
            let path = path
                .strip_suffix(".git")
                .unwrap_or(path)
                .trim_end_matches('/');
            let path = path.trim_start_matches('/');
            if host.is_empty() || path.is_empty() {
                return Err(ParseError::MalformedSshUrl {
                    input: input.to_string(),
                });
            }
            Ok(NormalizedSource::Git(SourceUrl::from(format!(
                "{host}/{path}"
            ))))
        }
        SourceFormat::BareDomain => {
            let stripped = input.strip_suffix(".git").unwrap_or(input);
            let stripped = stripped.trim_end_matches('/');
            if stripped.is_empty() || !stripped.contains('/') {
                return Err(ParseError::EmptyUrlPath {
                    input: input.to_string(),
                });
            }
            Ok(NormalizedSource::Git(SourceUrl::from(stripped.to_string())))
        }
        SourceFormat::Unknown => Err(ParseError::UnrecognizedFormat {
            input: input.to_string(),
        }),
    }
}

/// Derive source display name from normalized source.
pub fn derive_name(source: &NormalizedSource) -> Result<String, ParseError> {
    let name = match source {
        NormalizedSource::Git(url) => url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ParseError::CannotDeriveName {
                input: url.to_string(),
            })?,
        NormalizedSource::Path(path) => path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ParseError::CannotDeriveName {
                input: path.display().to_string(),
            })?,
    };

    Ok(name.to_string())
}

/// Parse a source specifier into a normalized structured value.
pub fn parse(input: &str) -> Result<ParsedSourceSpec, ParseError> {
    let format = classify(input);
    if format == SourceFormat::Unknown {
        return Err(ParseError::UnrecognizedFormat {
            input: input.to_string(),
        });
    }

    let (base, version) = split_version(input, format);
    let normalized = normalize(base, format)?;
    let name = derive_name(&normalized)?;

    let (url, path) = match normalized {
        NormalizedSource::Git(url) => (Some(url), None),
        NormalizedSource::Path(path) => (None, Some(path)),
    };

    Ok(ParsedSourceSpec {
        format,
        raw: input.to_string(),
        url,
        path,
        version: version.map(str::to_string),
        name,
    })
}

fn strip_suffix_at(input: &str) -> &str {
    match input.rsplit_once('@') {
        Some((base, suffix)) if !suffix.is_empty() => base,
        _ => input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn classify_detects_known_formats() {
        assert_eq!(classify("./local"), SourceFormat::LocalPath);
        assert_eq!(classify("owner/repo"), SourceFormat::GitHubShorthand);
        assert_eq!(
            classify("https://github.com/org/repo"),
            SourceFormat::HttpsUrl
        );
        assert_eq!(
            classify("git@github.com:org/repo.git"),
            SourceFormat::SshUrl
        );
        assert_eq!(classify("github.com/org/repo"), SourceFormat::BareDomain);
        assert_eq!(classify("invalid"), SourceFormat::Unknown);
    }

    #[test]
    fn split_version_only_for_supported_formats() {
        assert_eq!(
            split_version("owner/repo@v1", SourceFormat::GitHubShorthand),
            ("owner/repo", Some("v1"))
        );
        assert_eq!(
            split_version("https://github.com/org/repo@v2", SourceFormat::HttpsUrl),
            ("https://github.com/org/repo", Some("v2"))
        );
        assert_eq!(
            split_version("github.com/org/repo@latest", SourceFormat::BareDomain),
            ("github.com/org/repo", Some("latest"))
        );
        assert_eq!(
            split_version("git@github.com:org/repo.git@v1", SourceFormat::SshUrl),
            ("git@github.com:org/repo.git@v1", None)
        );
        assert_eq!(
            split_version("./local@v1", SourceFormat::LocalPath),
            ("./local@v1", None)
        );
    }

    #[test]
    fn normalize_handles_all_git_formats() {
        assert_eq!(
            normalize("owner/repo", SourceFormat::GitHubShorthand).unwrap(),
            NormalizedSource::Git(SourceUrl::from("github.com/owner/repo"))
        );
        assert_eq!(
            normalize("https://github.com/org/repo.git", SourceFormat::HttpsUrl).unwrap(),
            NormalizedSource::Git(SourceUrl::from("github.com/org/repo"))
        );
        assert_eq!(
            normalize("git@github.com:org/repo.git", SourceFormat::SshUrl).unwrap(),
            NormalizedSource::Git(SourceUrl::from("github.com/org/repo"))
        );
        assert_eq!(
            normalize("github.com/org/repo.git", SourceFormat::BareDomain).unwrap(),
            NormalizedSource::Git(SourceUrl::from("github.com/org/repo"))
        );
    }

    #[test]
    fn normalize_ssh_rejects_malformed() {
        let err = normalize("git@github.com", SourceFormat::SshUrl).unwrap_err();
        assert!(matches!(err, ParseError::MalformedSshUrl { .. }));
    }

    #[test]
    fn derive_name_from_git_and_path() {
        assert_eq!(
            derive_name(&NormalizedSource::Git(SourceUrl::from(
                "github.com/org/repo"
            )))
            .unwrap(),
            "repo"
        );
        assert_eq!(
            derive_name(&NormalizedSource::Path(PathBuf::from("../my-agents"))).unwrap(),
            "my-agents"
        );
    }

    #[test]
    fn parse_matrix_examples() {
        struct Case {
            input: &'static str,
            format: SourceFormat,
            url: Option<&'static str>,
            path: Option<&'static str>,
            version: Option<&'static str>,
            name: &'static str,
        }

        let cases = [
            Case {
                input: "./my-agents",
                format: SourceFormat::LocalPath,
                url: None,
                path: Some("./my-agents"),
                version: None,
                name: "my-agents",
            },
            Case {
                input: "haowjy/meridian-base",
                format: SourceFormat::GitHubShorthand,
                url: Some("github.com/haowjy/meridian-base"),
                path: None,
                version: None,
                name: "meridian-base",
            },
            Case {
                input: "haowjy/meridian-base@v1.0",
                format: SourceFormat::GitHubShorthand,
                url: Some("github.com/haowjy/meridian-base"),
                path: None,
                version: Some("v1.0"),
                name: "meridian-base",
            },
            Case {
                input: "https://github.com/org/repo.git",
                format: SourceFormat::HttpsUrl,
                url: Some("github.com/org/repo"),
                path: None,
                version: None,
                name: "repo",
            },
            Case {
                input: "https://github.com/org/repo@v2",
                format: SourceFormat::HttpsUrl,
                url: Some("github.com/org/repo"),
                path: None,
                version: Some("v2"),
                name: "repo",
            },
            Case {
                input: "git@github.com:org/repo.git",
                format: SourceFormat::SshUrl,
                url: Some("github.com/org/repo"),
                path: None,
                version: None,
                name: "repo",
            },
            Case {
                input: "git@github.com:org/repo.git@v1.0",
                format: SourceFormat::SshUrl,
                url: Some("github.com/org/repo.git@v1.0"),
                path: None,
                version: None,
                name: "repo.git@v1.0",
            },
            Case {
                input: "github.com/haowjy/meridian-base",
                format: SourceFormat::BareDomain,
                url: Some("github.com/haowjy/meridian-base"),
                path: None,
                version: None,
                name: "meridian-base",
            },
            Case {
                input: "github.com/haowjy/meridian-base@latest",
                format: SourceFormat::BareDomain,
                url: Some("github.com/haowjy/meridian-base"),
                path: None,
                version: Some("latest"),
                name: "meridian-base",
            },
        ];

        for case in cases {
            let parsed = parse(case.input).unwrap();
            assert_eq!(
                parsed.format, case.format,
                "format mismatch for {}",
                case.input
            );
            assert_eq!(
                parsed.url.as_deref(),
                case.url,
                "url mismatch for {}",
                case.input
            );
            assert_eq!(
                parsed.path.as_deref(),
                case.path.map(Path::new),
                "path mismatch for {}",
                case.input
            );
            assert_eq!(
                parsed.version.as_deref(),
                case.version,
                "version mismatch for {}",
                case.input
            );
            assert_eq!(parsed.name, case.name, "name mismatch for {}", case.input);
        }
    }

    #[test]
    fn parse_unknown_returns_error() {
        let err = parse("source").unwrap_err();
        assert!(matches!(err, ParseError::UnrecognizedFormat { .. }));
    }
}
