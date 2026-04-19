//! `mars version <bump|X.Y.Z> [--push]` — bump package version, commit, and tag.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

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

    run_git(
        &ctx.project_root,
        ["add", "mars.toml"],
        "git add mars.toml".to_string(),
    )?;
    run_git(
        &ctx.project_root,
        ["commit", "-m", &tag],
        format!("git commit -m {tag}"),
    )?;
    run_git(
        &ctx.project_root,
        ["tag", "-a", &tag, "-m", &tag],
        format!("git tag -a {tag} -m {tag}"),
    )?;

    if args.push {
        let branch = current_branch(&ctx.project_root)?;
        run_git(
            &ctx.project_root,
            ["push", "origin", &branch],
            format!("git push origin {branch}"),
        )?;
        run_git(
            &ctx.project_root,
            ["push", "origin", &tag],
            format!("git push origin {tag}"),
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
    let output = run_git(
        project_root,
        ["status", "--porcelain"],
        "git status --porcelain".to_string(),
    )?;

    if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
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
    let output = run_git(
        project_root,
        ["tag", "--list", tag],
        format!("git tag --list {tag}"),
    )?;

    let exists = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == tag);

    if exists {
        return Err(ConfigError::Invalid {
            message: format!("tag `{tag}` already exists"),
        }
        .into());
    }

    Ok(())
}

fn current_branch(project_root: &Path) -> Result<String, MarsError> {
    let output = run_git(
        project_root,
        ["rev-parse", "--abbrev-ref", "HEAD"],
        "git rev-parse --abbrev-ref HEAD".to_string(),
    )?;

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return Err(ConfigError::Invalid {
            message: "cannot push from detached HEAD".to_string(),
        }
        .into());
    }

    Ok(branch)
}

fn run_git<I, S>(project_root: &Path, args: I, display_command: String) -> Result<Output, MarsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(project_root)
        .args(args)
        .output()
        .map_err(|err| MarsError::GitCli {
            command: display_command.clone(),
            message: err.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("command exited with status {}", output.status)
        };

        return Err(MarsError::GitCli {
            command: display_command,
            message,
        });
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
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
