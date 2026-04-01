//! `mars add <source>` — add or update a source, then sync.

use std::path::Path;

use crate::config::{FilterConfig, SourceEntry};
use crate::error::{ConfigError, MarsError};
use crate::source::parse;
use crate::sync::{ConfigMutation, ResolutionMode, SyncOptions, SyncRequest};
use crate::types::{ItemName, SourceName};

use super::output;

/// Arguments for `mars add`.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Source specifier: owner/repo, owner/repo@version, URL, or local path.
    pub source: String,

    /// Only install specific agents from this source.
    #[arg(long, value_delimiter = ',')]
    pub agents: Vec<String>,

    /// Only install specific skills from this source.
    #[arg(long, value_delimiter = ',')]
    pub skills: Vec<String>,

    /// Exclude specific items from this source.
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,
}

/// Parsed source specifier.
struct ParsedSource {
    name: SourceName,
    entry: SourceEntry,
}

/// Run `mars add`.
pub fn run(args: &AddArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    // Parse source specifier
    let parsed = parse_source_specifier(&args.source)?;

    // Build SourceEntry with filters
    let entry = SourceEntry {
        url: parsed.entry.url,
        path: parsed.entry.path,
        version: parsed.entry.version,
        filter: FilterConfig {
            agents: if args.agents.is_empty() {
                None
            } else {
                Some(
                    args.agents
                        .iter()
                        .map(|v| ItemName::from(v.as_str()))
                        .collect(),
                )
            },
            skills: if args.skills.is_empty() {
                None
            } else {
                Some(
                    args.skills
                        .iter()
                        .map(|v| ItemName::from(v.as_str()))
                        .collect(),
                )
            },
            exclude: if args.exclude.is_empty() {
                None
            } else {
                Some(
                    args.exclude
                        .iter()
                        .map(|v| ItemName::from(v.as_str()))
                        .collect(),
                )
            },
            rename: None,
        },
    };

    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: Some(ConfigMutation::UpsertSource {
            name: parsed.name.clone(),
            entry,
        }),
        options: SyncOptions::default(),
    };

    // Check if source already exists before executing (for accurate messaging).
    let already_exists = crate::config::load(root)
        .map(|c| c.sources.contains_key(&parsed.name))
        .unwrap_or(false);

    let report = crate::sync::execute(root, &request)?;

    if !json {
        if already_exists {
            output::print_warn(&format!(
                "source `{}` already exists — updated",
                parsed.name
            ));
        } else {
            output::print_info(&format!("added source `{}`", parsed.name));
        }
    }

    output::print_sync_report(&report, json);

    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}

/// Parse a source specifier string into a name + SourceEntry.
///
/// Formats:
/// - `owner/repo` → GitHub shorthand (no `.` in first segment, exactly one `/`)
/// - `owner/repo@version` → GitHub shorthand with version
/// - `github.com/owner/repo` → full git URL
/// - `https://github.com/owner/repo.git` → full git URL
/// - `./path` or `../path` or `/absolute` → local path
fn parse_source_specifier(spec: &str) -> Result<ParsedSource, MarsError> {
    let parsed = parse::parse(spec).map_err(|e| {
        MarsError::Config(ConfigError::Invalid {
            message: e.to_string(),
        })
    })?;

    Ok(ParsedSource {
        name: SourceName::from(parsed.name),
        entry: SourceEntry {
            url: parsed.url,
            path: parsed.path,
            version: parsed.version,
            filter: FilterConfig::default(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_shorthand() {
        let parsed = parse_source_specifier("haowjy/meridian-base").unwrap();
        assert_eq!(parsed.name, "meridian-base");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/haowjy/meridian-base")
        );
        assert!(parsed.entry.path.is_none());
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_github_shorthand_with_version() {
        let parsed = parse_source_specifier("haowjy/meridian-base@v0.5.0").unwrap();
        assert_eq!(parsed.name, "meridian-base");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/haowjy/meridian-base")
        );
        assert_eq!(parsed.entry.version.as_deref(), Some("v0.5.0"));
    }

    #[test]
    fn parse_full_url() {
        let parsed = parse_source_specifier("github.com/haowjy/meridian-dev-workflow@v2").unwrap();
        assert_eq!(parsed.name, "meridian-dev-workflow");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/haowjy/meridian-dev-workflow")
        );
        assert_eq!(parsed.entry.version.as_deref(), Some("v2"));
    }

    #[test]
    fn parse_https_url() {
        let parsed = parse_source_specifier("https://github.com/someone/cool-agents.git").unwrap();
        assert_eq!(parsed.name, "cool-agents");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/someone/cool-agents")
        );
    }

    #[test]
    fn parse_ssh_url() {
        let parsed = parse_source_specifier("git@github.com:someone/cool-agents.git").unwrap();
        assert_eq!(parsed.name, "cool-agents");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/someone/cool-agents")
        );
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_ssh_url_keeps_at_suffix_in_path() {
        let parsed = parse_source_specifier("git@github.com:someone/cool-agents.git@v2").unwrap();
        assert_eq!(parsed.name, "cool-agents.git@v2");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/someone/cool-agents.git@v2")
        );
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_local_path_relative() {
        let parsed = parse_source_specifier("./my-agents").unwrap();
        assert_eq!(parsed.name, "my-agents");
        assert!(parsed.entry.url.is_none());
        assert_eq!(parsed.entry.path.as_deref(), Some(Path::new("./my-agents")));
    }

    #[test]
    fn parse_local_path_parent() {
        let parsed = parse_source_specifier("../meridian-dev-workflow").unwrap();
        assert_eq!(parsed.name, "meridian-dev-workflow");
        assert!(parsed.entry.url.is_none());
        assert_eq!(
            parsed.entry.path.as_deref(),
            Some(Path::new("../meridian-dev-workflow"))
        );
    }

    #[test]
    fn parse_local_path_absolute() {
        let parsed = parse_source_specifier("/home/dev/agents").unwrap();
        assert_eq!(parsed.name, "agents");
        assert!(parsed.entry.url.is_none());
        assert_eq!(
            parsed.entry.path.as_deref(),
            Some(Path::new("/home/dev/agents"))
        );
    }
}
