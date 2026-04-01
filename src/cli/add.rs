//! `mars add <source>` — add or update a source, then sync.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::config::{Config, Settings, SourceEntry};
use crate::error::MarsError;

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
    name: String,
    entry: SourceEntry,
}

/// Run `mars add`.
pub fn run(args: &AddArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    // Auto-init if needed
    let config_path = root.join("agents.toml");
    if !config_path.exists() {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join(".mars"))?;
        let config = Config {
            sources: IndexMap::new(),
            settings: Settings {},
        };
        crate::config::save(root, &config)?;
    }

    // Parse source specifier
    let parsed = parse_source_specifier(&args.source)?;

    // Build SourceEntry with filters
    let entry = SourceEntry {
        url: parsed.entry.url,
        path: parsed.entry.path,
        version: parsed.entry.version,
        agents: if args.agents.is_empty() {
            None
        } else {
            Some(args.agents.clone())
        },
        skills: if args.skills.is_empty() {
            None
        } else {
            Some(args.skills.clone())
        },
        exclude: if args.exclude.is_empty() {
            None
        } else {
            Some(args.exclude.clone())
        },
        rename: None,
    };

    // Load existing config and upsert
    let mut config = crate::config::load(root)?;
    config.sources.insert(parsed.name.clone(), entry);
    crate::config::save(root, &config)?;

    if !json {
        output::print_info(&format!("added source `{}`", parsed.name));
    }

    // Run sync
    let report = super::sync::run_sync(root, false, false, false)?;

    output::print_sync_report(&report, json);

    if report.has_conflicts() {
        Ok(1)
    } else {
        Ok(0)
    }
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
    // Split off @version if present
    let (base, version) = if let Some(at_pos) = spec.rfind('@') {
        let (b, v) = spec.split_at(at_pos);
        let ver = &v[1..]; // skip '@'
        if ver.is_empty() {
            (spec.to_string(), None)
        } else {
            (b.to_string(), Some(ver.to_string()))
        }
    } else {
        (spec.to_string(), None)
    };

    // Local path: starts with `.`, `/`, or `~`
    if base.starts_with('.')
        || base.starts_with('/')
        || base.starts_with('~')
    {
        let path = PathBuf::from(&base);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "local".to_string());

        return Ok(ParsedSource {
            name,
            entry: SourceEntry {
                url: None,
                path: Some(path),
                version: None, // local paths are unversioned
                agents: None,
                skills: None,
                exclude: None,
                rename: None,
            },
        });
    }

    // GitHub shorthand: owner/repo (no `.` in first segment, exactly one `/`)
    let parts: Vec<&str> = base.split('/').collect();
    let is_github_shorthand = parts.len() == 2
        && !parts[0].contains('.')
        && !parts[0].is_empty()
        && !parts[1].is_empty();

    if is_github_shorthand {
        let url = format!("github.com/{}", base);
        let name = parts[1].to_string();

        return Ok(ParsedSource {
            name,
            entry: SourceEntry {
                url: Some(url),
                path: None,
                version,
                agents: None,
                skills: None,
                exclude: None,
                rename: None,
            },
        });
    }

    // Full URL: contains a `.` in the domain part
    // Strip https:// or git:// prefix, strip .git suffix
    let cleaned = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("git://")
        .trim_end_matches(".git");

    // Extract name from last path segment
    let name = cleaned
        .rsplit('/')
        .next()
        .unwrap_or("source")
        .to_string();

    Ok(ParsedSource {
        name,
        entry: SourceEntry {
            url: Some(cleaned.to_string()),
            path: None,
            version,
            agents: None,
            skills: None,
            exclude: None,
            rename: None,
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
        let parsed =
            parse_source_specifier("github.com/haowjy/meridian-dev-workflow@v2").unwrap();
        assert_eq!(parsed.name, "meridian-dev-workflow");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/haowjy/meridian-dev-workflow")
        );
        assert_eq!(parsed.entry.version.as_deref(), Some("v2"));
    }

    #[test]
    fn parse_https_url() {
        let parsed =
            parse_source_specifier("https://github.com/someone/cool-agents.git").unwrap();
        assert_eq!(parsed.name, "cool-agents");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("github.com/someone/cool-agents")
        );
    }

    #[test]
    fn parse_local_path_relative() {
        let parsed = parse_source_specifier("./my-agents").unwrap();
        assert_eq!(parsed.name, "my-agents");
        assert!(parsed.entry.url.is_none());
        assert_eq!(
            parsed.entry.path.as_deref(),
            Some(Path::new("./my-agents"))
        );
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
