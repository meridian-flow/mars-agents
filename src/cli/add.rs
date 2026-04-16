//! `mars add <dependency>` — add or update a dependency, then sync.

use crate::config::{DependencyEntry, FilterConfig};
use crate::error::{ConfigError, MarsError};
use crate::source::parse;
use crate::sync::{
    ConfigMutation, DependencyUpsertChange, ResolutionMode, SyncOptions, SyncRequest,
};
use crate::types::{ItemName, SourceName};

use super::output;

/// Arguments for `mars add`.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// Source specifiers (one or more): owner/repo, owner/repo@version, URL, or local path.
    #[arg(required = true)]
    pub sources: Vec<String>,

    /// Only install specific agents from this source.
    #[arg(long, value_delimiter = ',')]
    pub agents: Vec<String>,

    /// Only install specific skills from this source.
    #[arg(long, value_delimiter = ',')]
    pub skills: Vec<String>,

    /// Exclude specific items from this source.
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,

    /// Install only skills from this source (no agents).
    #[arg(long)]
    pub only_skills: bool,

    /// Install only agents (plus their transitive skill deps) from this source.
    #[arg(long)]
    pub only_agents: bool,
}

/// Parsed dependency specifier.
struct ParsedDependency {
    name: SourceName,
    entry: DependencyEntry,
}

/// Run `mars add`.
pub fn run(args: &AddArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    // Validate: filters require exactly one source
    let has_filters = !args.agents.is_empty()
        || !args.skills.is_empty()
        || !args.exclude.is_empty()
        || args.only_skills
        || args.only_agents;

    if has_filters && args.sources.len() > 1 {
        return Err(MarsError::InvalidRequest {
            message: "filters may only be used when adding exactly one source".to_string(),
        });
    }

    // Validate filter flag combinations early
    let filter_config = build_filter_config(args);
    crate::config::validate_filter(&filter_config, "cli")?;

    // Build mutations for all sources
    let mutations: Vec<(SourceName, DependencyEntry)> = args
        .sources
        .iter()
        .map(|source| {
            let parsed = parse_dependency_specifier(source)?;
            let entry = DependencyEntry {
                url: parsed.entry.url,
                path: parsed.entry.path,
                subpath: parsed.entry.subpath,
                version: parsed.entry.version,
                filter: filter_config.clone(),
            };
            Ok((parsed.name, entry))
        })
        .collect::<Result<Vec<_>, MarsError>>()?;

    // For single source, use direct mutation path
    // For multi-source, apply mutations sequentially then run one sync
    if mutations.len() == 1 {
        let (name, entry) = mutations.into_iter().next().unwrap();

        let request = SyncRequest {
            resolution: ResolutionMode::Normal,
            mutation: Some(ConfigMutation::UpsertDependency {
                name: name.clone(),
                entry,
            }),
            options: SyncOptions {
                force: false,
                dry_run: false,
                frozen: false,
                no_refresh_models: false,
            },
        };

        let report = crate::sync::execute(ctx, &request)?;

        if !json {
            print_dependency_messages(&report.dependency_changes);
        }

        output::print_sync_report(&report, json, true);
        return if report.has_conflicts() { Ok(1) } else { Ok(0) };
    }

    // Multi-source: send one batch mutation through sync pipeline.
    let request = SyncRequest {
        resolution: ResolutionMode::Normal,
        mutation: Some(ConfigMutation::BatchUpsert(mutations)),
        options: SyncOptions {
            force: false,
            dry_run: false,
            frozen: false,
            no_refresh_models: false,
        },
    };

    let report = crate::sync::execute(ctx, &request)?;

    if !json {
        print_dependency_messages(&report.dependency_changes);
    }

    output::print_sync_report(&report, json, true);
    if report.has_conflicts() { Ok(1) } else { Ok(0) }
}

/// Build FilterConfig from CLI args.
fn build_filter_config(args: &AddArgs) -> FilterConfig {
    FilterConfig {
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
        only_skills: args.only_skills,
        only_agents: args.only_agents,
    }
}

/// Parse a dependency specifier string into a name + DependencyEntry.
///
/// Formats:
/// - `owner/repo` → GitHub shorthand (no `.` in first segment, exactly one `/`)
/// - `owner/repo@version` → GitHub shorthand with version
/// - `github.com/owner/repo` → full git URL
/// - `https://github.com/owner/repo.git` → full git URL
/// - `./path` or `../path` or `/absolute` → local path
fn parse_dependency_specifier(spec: &str) -> Result<ParsedDependency, MarsError> {
    let parsed = parse::parse(spec).map_err(|e| {
        MarsError::Config(ConfigError::Invalid {
            message: e.to_string(),
        })
    })?;

    Ok(ParsedDependency {
        name: SourceName::from(parsed.name),
        entry: DependencyEntry {
            url: parsed.url,
            path: parsed.path,
            subpath: None,
            version: parsed.version,
            filter: FilterConfig::default(),
        },
    })
}

fn print_dependency_messages(changes: &[DependencyUpsertChange]) {
    for change in changes {
        if change.already_exists {
            output::print_warn(&format!(
                "dependency `{}` already exists — updated",
                change.name
            ));
            if let Some(old_filter) = &change.old_filter
                && old_filter != &change.new_filter
            {
                output::print_info(&format!(
                    "filters changed: {} → {}",
                    format_filter(old_filter),
                    format_filter(&change.new_filter)
                ));
            }
        } else {
            output::print_info(&format!("added dependency `{}`", change.name));
        }
    }
}

fn format_filter(filter: &FilterConfig) -> String {
    if filter.only_skills {
        return "only_skills=true".to_string();
    }
    if filter.only_agents {
        return "only_agents=true".to_string();
    }

    let mut parts = Vec::new();
    if let Some(agents) = &filter.agents {
        parts.push(format!("agents=[{}]", format_item_names(agents)));
    }
    if let Some(skills) = &filter.skills {
        parts.push(format!("skills=[{}]", format_item_names(skills)));
    }
    if let Some(exclude) = &filter.exclude {
        parts.push(format!("exclude=[{}]", format_item_names(exclude)));
    }

    if parts.is_empty() {
        "all".to_string()
    } else {
        parts.join(", ")
    }
}

fn format_item_names(items: &[ItemName]) -> String {
    items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::DependencyUpsertChange;
    use std::path::Path;

    #[test]
    fn parse_github_shorthand() {
        let parsed = parse_dependency_specifier("meridian-flow/meridian-base").unwrap();
        assert_eq!(parsed.name, "meridian-base");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("https://github.com/meridian-flow/meridian-base")
        );
        assert!(parsed.entry.path.is_none());
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_github_shorthand_with_version() {
        let parsed = parse_dependency_specifier("meridian-flow/meridian-base@v0.5.0").unwrap();
        assert_eq!(parsed.name, "meridian-base");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("https://github.com/meridian-flow/meridian-base")
        );
        assert_eq!(parsed.entry.version.as_deref(), Some("v0.5.0"));
    }

    #[test]
    fn parse_full_url() {
        let parsed =
            parse_dependency_specifier("github.com/meridian-flow/meridian-dev-workflow@v2")
                .unwrap();
        assert_eq!(parsed.name, "meridian-dev-workflow");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("https://github.com/meridian-flow/meridian-dev-workflow")
        );
        assert_eq!(parsed.entry.version.as_deref(), Some("v2"));
    }

    #[test]
    fn parse_https_url() {
        let parsed =
            parse_dependency_specifier("https://github.com/someone/cool-agents.git").unwrap();
        assert_eq!(parsed.name, "cool-agents");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("https://github.com/someone/cool-agents")
        );
    }

    #[test]
    fn parse_ssh_url() {
        let parsed = parse_dependency_specifier("git@github.com:someone/cool-agents.git").unwrap();
        assert_eq!(parsed.name, "cool-agents");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("git@github.com:someone/cool-agents.git")
        );
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_ssh_url_keeps_at_suffix_in_path() {
        let parsed =
            parse_dependency_specifier("git@github.com:someone/cool-agents.git@v2").unwrap();
        assert_eq!(parsed.name, "cool-agents.git@v2");
        assert_eq!(
            parsed.entry.url.as_deref(),
            Some("git@github.com:someone/cool-agents.git@v2")
        );
        assert!(parsed.entry.version.is_none());
    }

    #[test]
    fn parse_local_path_relative() {
        let parsed = parse_dependency_specifier("./my-agents").unwrap();
        assert_eq!(parsed.name, "my-agents");
        assert!(parsed.entry.url.is_none());
        assert_eq!(parsed.entry.path.as_deref(), Some(Path::new("./my-agents")));
    }

    #[test]
    fn parse_local_path_parent() {
        let parsed = parse_dependency_specifier("../meridian-dev-workflow").unwrap();
        assert_eq!(parsed.name, "meridian-dev-workflow");
        assert!(parsed.entry.url.is_none());
        assert_eq!(
            parsed.entry.path.as_deref(),
            Some(Path::new("../meridian-dev-workflow"))
        );
    }

    #[test]
    fn parse_local_path_absolute() {
        let parsed = parse_dependency_specifier("/home/dev/agents").unwrap();
        assert_eq!(parsed.name, "agents");
        assert!(parsed.entry.url.is_none());
        assert_eq!(
            parsed.entry.path.as_deref(),
            Some(Path::new("/home/dev/agents"))
        );
    }

    #[test]
    fn format_filter_all() {
        assert_eq!(format_filter(&FilterConfig::default()), "all");
    }

    #[test]
    fn format_filter_only_modes() {
        assert_eq!(
            format_filter(&FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            }),
            "only_skills=true"
        );
        assert_eq!(
            format_filter(&FilterConfig {
                only_agents: true,
                ..FilterConfig::default()
            }),
            "only_agents=true"
        );
    }

    #[test]
    fn format_filter_lists() {
        assert_eq!(
            format_filter(&FilterConfig {
                agents: Some(vec!["reviewer".into(), "planner".into()]),
                ..FilterConfig::default()
            }),
            "agents=[reviewer,planner]"
        );
        assert_eq!(
            format_filter(&FilterConfig {
                exclude: Some(vec!["legacy".into()]),
                ..FilterConfig::default()
            }),
            "exclude=[legacy]"
        );
    }

    #[test]
    fn detects_filter_change_for_message() {
        let old_filter = FilterConfig {
            agents: Some(vec!["reviewer".into()]),
            ..FilterConfig::default()
        };
        let change = DependencyUpsertChange {
            name: "ops".into(),
            already_exists: true,
            old_version: Some("v0.1.0".into()),
            new_version: Some("v0.1.0".into()),
            old_filter: Some(old_filter.clone()),
            new_filter: FilterConfig {
                only_skills: true,
                ..FilterConfig::default()
            },
        };
        assert_ne!(change.old_filter.as_ref(), Some(&change.new_filter));
        assert_eq!(format_filter(&old_filter), "agents=[reviewer]");
        assert_eq!(format_filter(&change.new_filter), "only_skills=true");
    }
}
