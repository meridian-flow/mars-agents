mod common;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;
use toml::Value;

use common::*;

#[test]
fn init_creates_agents_toml() {
    let dir = TempDir::new().unwrap();

    mars()
        .args(["init", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized"));

    let agents_dir = dir.child(".agents");
    assert!(dir.child("mars.toml").exists());
    assert!(dir.child(".mars").exists());
    assert!(!dir.child(".gitignore").exists());
    assert!(agents_dir.exists());
}

#[test]
fn init_twice_is_idempotent() {
    let dir = TempDir::new().unwrap();

    mars()
        .args(["init", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success();

    // Second init should succeed (idempotent) with info message
    mars()
        .args(["init", "--root", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already initialized"));
}

#[test]
fn add_local_source_and_sync() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "test-source",
        &[("coder", "# Coder agent")],
        &[("planning", "# Planning skill")],
    );

    // Init
    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Add
    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Verify files installed
    assert!(agents_dir.child("agents").child("coder.md").exists());
    assert!(
        agents_dir
            .child("skills")
            .child("planning")
            .child("SKILL.md")
            .exists()
    );

    // Verify lock file exists
    assert!(dir.child("project").child("mars.lock").exists());
}

#[test]
fn add_auto_inits_project_when_root_has_no_mars_toml() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "bootstrap-source", &[("coder", "# Coder agent")], &[]);
    let project = dir.child("project");

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-initialized"));

    assert!(project.child("mars.toml").exists());
    assert!(project.child(".mars").exists());
    assert!(
        project
            .child(".agents")
            .child("agents")
            .child("coder.md")
            .exists()
    );
    assert!(project.child("mars.lock").exists());
}

#[test]
fn link_auto_inits_project_when_root_has_no_mars_toml() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");

    mars()
        .args([
            "link",
            ".claude",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-initialized"));

    assert!(project.child("mars.toml").exists());
    assert!(project.child(".mars").exists());

    let config_content = fs::read_to_string(project.child("mars.toml").path()).unwrap();
    assert!(
        config_content.contains(".claude"),
        "expected linked target to be persisted; config:\n{config_content}"
    );
}

#[test]
fn sync_without_project_still_errors_instead_of_auto_init() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no mars.toml found"));

    assert!(!project.child("mars.toml").exists());
}

#[test]
fn sync_idempotent() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("reviewer", "# Reviewer")], &[]);

    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Second sync should report up to date
    mars()
        .args([
            "sync",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));
}

#[test]
fn remove_prunes_files() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder agent")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(agents_dir.child("agents").child("coder.md").exists());

    // Remove the source
    mars()
        .args([
            "remove",
            "base",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // File should be pruned
    assert!(!agents_dir.child("agents").child("coder.md").exists());
}

#[test]
fn list_shows_installed_items() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder")],
        &[("planning", "# Planning")],
    );

    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Catalog view (default)
    mars()
        .args([
            "list",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("coder"))
        .stdout(predicate::str::contains("AGENTS"))
        .stdout(predicate::str::contains("SKILLS"));

    // Status view
    mars()
        .args([
            "list",
            "--status",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn why_traces_skill_to_source() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "---\nskills:\n  - planning\n---\n# Coder agent\n")],
        &[("planning", "# Planning skill")],
    );

    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "why",
            "planning",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("planning (skill)"))
        .stdout(predicate::str::contains("provided by: base"))
        .stdout(predicate::str::contains("agents/coder.md"));
}

#[test]
fn root_discovery_from_subdir() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("agent", "# Agent")], &[]);

    // Init and add
    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Create a subdirectory
    let subdir = dir.child("project").child("subdir").child("deep");
    subdir.create_dir_all().unwrap();

    // Run list from the subdirectory — should find .agents/ by walking up
    mars()
        .args(["list"])
        .current_dir(subdir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("agent"));
}

#[test]
fn json_output_valid() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // List with --json
    let output = mars()
        .args([
            "list",
            "--json",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Should be valid JSON with agents/skills keys
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.is_object());
    assert!(parsed.get("agents").is_some());
    assert!(parsed.get("skills").is_some());
    let agents = parsed["agents"].as_array().unwrap();
    assert!(!agents.is_empty());
    assert!(agents[0].get("name").is_some());

    // Status JSON should still return array format
    let output = mars()
        .args([
            "list",
            "--status",
            "--json",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(parsed.is_array());
    let arr = parsed.as_array().unwrap();
    assert!(!arr.is_empty());
    assert!(arr[0].get("source").is_some());
    assert!(arr[0].get("status").is_some());
}

#[test]
fn doctor_reports_healthy_state() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let _agents_dir = dir.child("project").child(".agents");
    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    fs::write(dir.child("project").child(".gitignore").path(), ".mars/\n").unwrap();

    mars()
        .args([
            "doctor",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("all checks passed"));
}

#[test]
fn doctor_warns_when_mars_not_gitignored() {
    let dir = TempDir::new().unwrap();

    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "doctor",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(".mars/ is not in .gitignore"));
}

#[test]
fn help_shows_all_commands() {
    mars()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("sync"))
        .stdout(predicate::str::contains("remove"))
        .stdout(predicate::str::contains("upgrade"))
        .stdout(predicate::str::contains("outdated"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("why"))
        .stdout(predicate::str::contains("rename"))
        .stdout(predicate::str::contains("resolve"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("repair"));
}

#[test]
fn add_nonexistent_path_does_not_pollute_config() {
    let dir = TempDir::new().unwrap();
    let _agents_dir = dir.child("project").child(".agents");

    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let missing = dir.child("does-not-exist").path().to_path_buf();
    mars()
        .args([
            "add",
            missing.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .failure();

    let config_content =
        fs::read_to_string(dir.child("project").child("mars.toml").path()).unwrap();
    let config: Value = toml::from_str(&config_content).unwrap();
    let deps = config["dependencies"].as_table().unwrap();
    assert!(
        deps.is_empty(),
        "expected no dependencies after failed add, got: {config_content}"
    );
}

#[test]
fn upgrade_command_is_available() {
    mars()
        .args(["upgrade", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("upgrade"));
}

#[test]
fn init_with_root_uses_resolved_root_path_in_message() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("proj");
    project.create_dir_all().unwrap();
    let root = project.path().to_path_buf();

    mars()
        .args(["init", "--root", root.to_str().unwrap()])
        .assert()
        .success();

    // Second init should be idempotent
    mars()
        .args(["init", "--root", root.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already initialized"));
}
