//! `mars version <bump|X.Y.Z> [--push]` — bump package version, commit, and tag.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::{BuildMetadata, Prerelease, Version};

use crate::error::{ConfigError, MarsError};

use super::{check, output};

/// Arguments for `mars version`.
#[derive(Debug, clap::Args)]
pub struct VersionArgs {
    /// Version bump: patch, minor, major, or explicit X.Y.Z
    pub bump: String,
    /// Push branch and tag to origin after versioning
    #[arg(long)]
    pub push: bool,
}

/// Run `mars version`.
pub fn run(args: &VersionArgs, ctx: &super::MarsContext, json: bool) -> Result<i32, MarsError> {
    require_clean_working_tree(&ctx.project_root)?;
    require_package_check(&ctx.project_root)?;

    let mut config = crate::config::load(&ctx.project_root)?;
    let package = config
        .package
        .as_mut()
        .ok_or_else(|| ConfigError::Invalid {
            message: "mars.toml must contain [package] with name and version".to_string(),
        })?;

    if package.name.trim().is_empty() {
        return Err(ConfigError::Invalid {
            message: "[package].name must not be empty".to_string(),
        }
        .into());
    }

    let current = parse_release_version(&package.version, "[package].version")?;
    let next = resolve_next_version(&args.bump, &current)?;

    if next == current {
        return Err(ConfigError::Invalid {
            message: format!(
                "new version `{}` matches current version `{}`",
                next, package.version
            ),
        }
        .into());
    }

    let next_version = next.to_string();
    let tag = format!("v{next_version}");

    ensure_tag_not_exists(&ctx.project_root, &tag)?;

    package.version = next_version.clone();
    crate::config::save(&ctx.project_root, &config)?;
    update_changelog_if_present(&ctx.project_root, &next_version)?;

    crate::platform::process::run_git(
        &["add", "mars.toml"],
        &ctx.project_root,
        "git add mars.toml",
    )?;
    if ctx.project_root.join("CHANGELOG.md").is_file() {
        crate::platform::process::run_git(
            &["add", "CHANGELOG.md"],
            &ctx.project_root,
            "git add CHANGELOG.md",
        )?;
    }
    crate::platform::process::run_git(
        &["commit", "-m", &tag],
        &ctx.project_root,
        &format!("git commit -m {tag}"),
    )?;
    crate::platform::process::run_git(
        &["tag", "-a", &tag, "-m", &tag],
        &ctx.project_root,
        &format!("git tag -a {tag} -m {tag}"),
    )?;

    if args.push {
        let branch = current_branch(&ctx.project_root)?;
        crate::platform::process::run_git(
            &["push", "origin", &branch],
            &ctx.project_root,
            &format!("git push origin {branch}"),
        )?;
        crate::platform::process::run_git(
            &["push", "origin", &tag],
            &ctx.project_root,
            &format!("git push origin {tag}"),
        )?;
    }

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "version": next_version,
            "tag": tag,
            "pushed": args.push,
        }));
    } else {
        println!("{tag}");
    }

    Ok(0)
}

fn require_clean_working_tree(project_root: &Path) -> Result<(), MarsError> {
    let output = crate::platform::process::run_git(
        &["status", "--porcelain"],
        project_root,
        "git status --porcelain",
    )?;

    if !output.is_empty() {
        return Err(ConfigError::Invalid {
            message: "working tree must be clean before running `mars version`".to_string(),
        }
        .into());
    }

    Ok(())
}

fn require_package_check(project_root: &Path) -> Result<(), MarsError> {
    // Skip check if this isn't a source package (no agents/, skills/, or SKILL.md)
    let has_agents = project_root.join("agents").is_dir();
    let has_skills = project_root.join("skills").is_dir();
    let has_root_skill = project_root.join("SKILL.md").is_file();
    if !has_agents && !has_skills && !has_root_skill {
        return Ok(());
    }

    let report = check::check_dir(project_root)?;
    if !report.errors.is_empty() {
        let mut message = "package check failed:".to_string();
        for error in &report.errors {
            message.push_str(&format!("\n  - {error}"));
        }
        return Err(ConfigError::Invalid { message }.into());
    }
    Ok(())
}

fn parse_release_version(value: &str, field_name: &str) -> Result<Version, MarsError> {
    let version = Version::parse(value).map_err(|_| ConfigError::Invalid {
        message: format!("{field_name} must be valid semver (X.Y.Z), got `{value}`"),
    })?;

    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(ConfigError::Invalid {
            message: format!("{field_name} must be plain X.Y.Z (no prerelease/build): `{value}`"),
        }
        .into());
    }

    Ok(version)
}

fn resolve_next_version(bump: &str, current: &Version) -> Result<Version, MarsError> {
    match bump {
        "patch" => Ok(Version {
            major: current.major,
            minor: current.minor,
            patch: current
                .patch
                .checked_add(1)
                .ok_or_else(|| ConfigError::Invalid {
                    message: "patch version overflow".to_string(),
                })?,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        }),
        "minor" => Ok(Version {
            major: current.major,
            minor: current
                .minor
                .checked_add(1)
                .ok_or_else(|| ConfigError::Invalid {
                    message: "minor version overflow".to_string(),
                })?,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        }),
        "major" => Ok(Version {
            major: current
                .major
                .checked_add(1)
                .ok_or_else(|| ConfigError::Invalid {
                    message: "major version overflow".to_string(),
                })?,
            minor: 0,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        }),
        explicit => parse_release_version(explicit, "requested version"),
    }
}

fn ensure_tag_not_exists(project_root: &Path, tag: &str) -> Result<(), MarsError> {
    let output = crate::platform::process::run_git(
        &["tag", "--list", tag],
        project_root,
        &format!("git tag --list {tag}"),
    )?;

    let exists = output.lines().any(|line| line.trim() == tag);

    if exists {
        return Err(ConfigError::Invalid {
            message: format!("tag `{tag}` already exists"),
        }
        .into());
    }

    Ok(())
}

fn current_branch(project_root: &Path) -> Result<String, MarsError> {
    let branch = crate::platform::process::run_git(
        &["rev-parse", "--abbrev-ref", "HEAD"],
        project_root,
        "git rev-parse --abbrev-ref HEAD",
    )?;
    if branch.is_empty() || branch == "HEAD" {
        return Err(ConfigError::Invalid {
            message: "cannot push from detached HEAD".to_string(),
        }
        .into());
    }

    Ok(branch)
}

fn update_changelog_if_present(project_root: &Path, next_version: &str) -> Result<(), MarsError> {
    let changelog_path = project_root.join("CHANGELOG.md");
    if !changelog_path.is_file() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&changelog_path)?;
    let Some(updated) = promote_unreleased_changelog(&content, next_version, &today_iso_date())
    else {
        return Ok(());
    };

    if updated.unreleased_was_empty {
        eprintln!("warning: CHANGELOG.md has no entries under [Unreleased]");
    }

    std::fs::write(changelog_path, updated.content)?;
    Ok(())
}

struct ChangelogPromotion {
    content: String,
    unreleased_was_empty: bool,
}

fn promote_unreleased_changelog(
    content: &str,
    next_version: &str,
    date: &str,
) -> Option<ChangelogPromotion> {
    let sections = content.split_inclusive('\n').collect::<Vec<_>>();

    let unreleased_index = sections
        .iter()
        .position(|line| is_unreleased_header(line.trim_end()))?;
    let next_section_index = sections
        .iter()
        .enumerate()
        .skip(unreleased_index + 1)
        .find_map(|(index, line)| {
            if line.trim_start().starts_with("## [") {
                Some(index)
            } else {
                None
            }
        })
        .unwrap_or(sections.len());

    let unreleased_was_empty =
        changelog_section_is_empty(&sections[unreleased_index + 1..next_section_index]);

    let mut promoted = String::new();
    for line in &sections[..unreleased_index] {
        promoted.push_str(line);
    }
    promoted.push_str("## [Unreleased]\n\n");
    promoted.push_str(&format!("## [{next_version}] - {date}\n"));
    for line in &sections[unreleased_index + 1..] {
        promoted.push_str(line);
    }

    Some(ChangelogPromotion {
        content: promoted,
        unreleased_was_empty,
    })
}

fn is_unreleased_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("## [")
        && trimmed.ends_with(']')
        && trimmed
            .trim_start_matches("## [")
            .trim_end_matches(']')
            .eq_ignore_ascii_case("unreleased")
}

fn changelog_section_is_empty(lines: &[&str]) -> bool {
    lines.iter().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || trimmed.starts_with("###")
    })
}

fn today_iso_date() -> String {
    let days_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    civil_date_from_days(days_since_epoch as i64)
}

fn civil_date_from_days(days_since_unix_epoch: i64) -> String {
    // Howard Hinnant's civil-from-days algorithm. Converts days since
    // 1970-01-01 to a proleptic Gregorian date without platform-specific APIs.
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn run_git_test<I, S>(cwd: &Path, args: I) -> String
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

    fn init_repo_with_mars_toml(mars_toml: &str) -> (TempDir, super::super::MarsContext) {
        let repo = TempDir::new().unwrap();
        run_git_test(repo.path(), ["init", "."]);
        run_git_test(repo.path(), ["config", "user.name", "Mars Test"]);
        run_git_test(repo.path(), ["config", "user.email", "mars@example.com"]);

        std::fs::create_dir_all(repo.path().join(".agents")).unwrap();
        std::fs::create_dir_all(repo.path().join("agents")).unwrap();
        std::fs::write(
            repo.path().join("agents/test-agent.md"),
            "---\nname: test-agent\ndescription: test\n---\n# Test",
        )
        .unwrap();
        std::fs::write(repo.path().join("mars.toml"), mars_toml).unwrap();
        run_git_test(repo.path(), ["add", "."]);
        run_git_test(repo.path(), ["commit", "-m", "init"]);

        let ctx = super::super::MarsContext::for_test(
            repo.path().to_path_buf(),
            repo.path().join(".agents"),
        );
        (repo, ctx)
    }

    #[test]
    fn parse_release_version_accepts_plain_semver() {
        let parsed = parse_release_version("1.2.3", "field").unwrap();
        assert_eq!(parsed.to_string(), "1.2.3");
    }

    #[test]
    fn parse_release_version_rejects_prerelease() {
        let err = parse_release_version("1.2.3-alpha.1", "field").unwrap_err();
        assert!(err.to_string().contains("plain X.Y.Z"));
    }

    #[test]
    fn resolve_next_version_bump_kinds() {
        let current = Version::parse("1.2.3").unwrap();

        assert_eq!(
            resolve_next_version("patch", &current).unwrap().to_string(),
            "1.2.4"
        );
        assert_eq!(
            resolve_next_version("minor", &current).unwrap().to_string(),
            "1.3.0"
        );
        assert_eq!(
            resolve_next_version("major", &current).unwrap().to_string(),
            "2.0.0"
        );
    }

    #[test]
    fn resolve_next_version_explicit() {
        let current = Version::parse("1.2.3").unwrap();
        assert_eq!(
            resolve_next_version("4.5.6", &current).unwrap().to_string(),
            "4.5.6"
        );
    }

    #[test]
    fn run_patch_updates_version_commits_and_tags() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let config = crate::config::load(repo.path()).unwrap();
        assert_eq!(config.package.unwrap().version, "0.1.1");

        let subject = run_git_test(repo.path(), ["log", "-1", "--pretty=%s"]);
        assert_eq!(subject, "v0.1.1");

        let tag = run_git_test(repo.path(), ["tag", "--list", "v0.1.1"]);
        assert_eq!(tag, "v0.1.1");
    }

    #[test]
    fn run_promotes_unreleased_in_changelog() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );
        std::fs::write(
            repo.path().join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n\n### Added\n- New feature X\n\n### Fixed\n- Bug Y\n",
        )
        .unwrap();
        run_git_test(repo.path(), ["add", "CHANGELOG.md"]);
        run_git_test(repo.path(), ["commit", "-m", "add changelog"]);

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let changelog = std::fs::read_to_string(repo.path().join("CHANGELOG.md")).unwrap();
        let today = today_iso_date();
        assert!(changelog.contains("## [Unreleased]\n\n## [0.1.1] - "));
        assert!(changelog.contains(&format!(
            "## [0.1.1] - {today}\n\n### Added\n- New feature X"
        )));
        assert!(changelog.contains("### Fixed\n- Bug Y"));

        let committed_files =
            run_git_test(repo.path(), ["show", "--name-only", "--pretty=", "HEAD"]);
        assert!(committed_files.lines().any(|line| line == "CHANGELOG.md"));
    }

    #[test]
    fn run_warns_on_empty_unreleased() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );
        std::fs::write(
            repo.path().join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n\n### Added\n\n### Fixed\n",
        )
        .unwrap();
        run_git_test(repo.path(), ["add", "CHANGELOG.md"]);
        run_git_test(repo.path(), ["commit", "-m", "add empty changelog"]);

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let changelog = std::fs::read_to_string(repo.path().join("CHANGELOG.md")).unwrap();
        assert!(changelog.contains("## [Unreleased]\n\n## [0.1.1] - "));
        assert!(changelog.contains("## [0.1.1] - "));
        assert!(
            promote_unreleased_changelog(
                "# Changelog\n\n## [Unreleased]\n\n### Added\n\n",
                "0.1.1",
                "2026-04-30"
            )
            .unwrap()
            .unreleased_was_empty
        );
    }

    #[test]
    fn run_succeeds_without_changelog() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let config = crate::config::load(repo.path()).unwrap();
        assert_eq!(config.package.unwrap().version, "0.1.1");
        assert!(!repo.path().join("CHANGELOG.md").exists());
    }

    #[test]
    fn run_changelog_preserves_existing_versions() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );
        let prior_section = "## [0.1.0] - 2026-04-01\n\n### Added\n- Initial release\n";
        std::fs::write(
            repo.path().join("CHANGELOG.md"),
            format!("# Changelog\n\n## [Unreleased]\n\n### Fixed\n- Bug Y\n\n{prior_section}"),
        )
        .unwrap();
        run_git_test(repo.path(), ["add", "CHANGELOG.md"]);
        run_git_test(repo.path(), ["commit", "-m", "add changelog"]);

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let changelog = std::fs::read_to_string(repo.path().join("CHANGELOG.md")).unwrap();
        assert!(changelog.contains("## [0.1.1] - "));
        assert!(changelog.contains("### Fixed\n- Bug Y"));
        assert!(changelog.ends_with(prior_section));
    }

    #[test]
    fn run_requires_clean_working_tree() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );
        std::fs::write(repo.path().join("dirty.txt"), "dirty\n").unwrap();

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let err = run(&args, &ctx, true).unwrap_err();
        assert!(err.to_string().contains("working tree must be clean"));

        let config = crate::config::load(repo.path()).unwrap();
        assert_eq!(config.package.unwrap().version, "0.1.0");
    }

    #[test]
    fn run_requires_package_section() {
        let (_repo, ctx) =
            init_repo_with_mars_toml("[dependencies]\nbase = { path = \"../base\" }\n");

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let err = run(&args, &ctx, true).unwrap_err();
        assert!(err.to_string().contains("must contain [package]"));
    }

    #[test]
    fn run_rejects_existing_tag() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );
        run_git_test(repo.path(), ["tag", "-a", "v0.1.1", "-m", "v0.1.1"]);

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: false,
        };

        let err = run(&args, &ctx, true).unwrap_err();
        assert!(err.to_string().contains("tag `v0.1.1` already exists"));
    }

    #[test]
    fn run_with_push_pushes_branch_and_tag_to_origin() {
        let (repo, ctx) = init_repo_with_mars_toml(
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        );

        let remote = TempDir::new().unwrap();
        run_git_test(remote.path(), ["init", "--bare", "."]);
        run_git_test(
            repo.path(),
            ["remote", "add", "origin", remote.path().to_str().unwrap()],
        );

        let args = VersionArgs {
            bump: "patch".to_string(),
            push: true,
        };

        let exit = run(&args, &ctx, true).unwrap();
        assert_eq!(exit, 0);

        let branch = run_git_test(repo.path(), ["rev-parse", "--abbrev-ref", "HEAD"]);
        let remote_branch = run_git_test(repo.path(), ["ls-remote", "--heads", "origin", &branch]);
        assert!(remote_branch.contains(&format!("refs/heads/{branch}")));

        let remote_tag = run_git_test(repo.path(), ["ls-remote", "--tags", "origin", "v0.1.1"]);
        assert!(remote_tag.contains("refs/tags/v0.1.1"));
    }
}
