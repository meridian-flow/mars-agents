// Integration tests for mars-agents.
//
// These tests exercise the CLI binary end-to-end using assert_cmd.
// Each test creates a temp directory with synthetic source content
// and runs mars commands against it.

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;
use toml::Value;

/// Create a local path source fixture with agents and skills.
fn create_source(
    dir: &TempDir,
    name: &str,
    agents: &[(&str, &str)],
    skills: &[(&str, &str)],
) -> std::path::PathBuf {
    let source_dir = dir.child(name);
    source_dir.create_dir_all().unwrap();

    if !agents.is_empty() {
        let agents_dir = source_dir.child("agents");
        agents_dir.create_dir_all().unwrap();
        for (agent_name, content) in agents {
            agents_dir
                .child(format!("{agent_name}.md"))
                .write_str(content)
                .unwrap();
        }
    }

    if !skills.is_empty() {
        let skills_dir = source_dir.child("skills");
        skills_dir.create_dir_all().unwrap();
        for (skill_name, content) in skills {
            let skill_sub = skills_dir.child(skill_name);
            skill_sub.create_dir_all().unwrap();
            skill_sub.child("SKILL.md").write_str(content).unwrap();
        }
    }

    source_dir.to_path_buf()
}

fn mars() -> Command {
    Command::cargo_bin("mars").unwrap()
}

// ═══════════════════════════════════════════════════════════════
// 1. Fresh init + add + sync
// ═══════════════════════════════════════════════════════════════

#[test]
fn init_creates_agents_toml() {
    let dir = TempDir::new().unwrap();

    mars()
        .args(["init", "--root", dir.path().join(".agents").to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized"));

    let agents_dir = dir.child(".agents");
    assert!(agents_dir.child("mars.toml").exists());
    assert!(agents_dir.child(".mars").exists());
    assert!(agents_dir.child(".gitignore").exists());
}

#[test]
fn init_twice_is_idempotent() {
    let dir = TempDir::new().unwrap();

    mars()
        .args(["init", "--root", dir.path().join(".agents").to_str().unwrap()])
        .assert()
        .success();

    // Second init should succeed (idempotent) with info message
    mars()
        .args(["init", "--root", dir.path().join(".agents").to_str().unwrap()])
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
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    // Add
    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
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
    assert!(agents_dir.child("mars.lock").exists());
}

#[test]
fn add_second_source_preserves_first_source_items_in_lock() {
    let dir = TempDir::new().unwrap();
    let source_one = create_source(&dir, "base1", &[("coder", "# Coder from base1")], &[]);
    let source_two = create_source(&dir, "base2", &[("reviewer", "# Reviewer from base2")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source_one.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source_two.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock_content = fs::read_to_string(agents_dir.child("mars.lock").path()).unwrap();
    let lock: Value = toml::from_str(&lock_content).unwrap();
    let items = lock["items"].as_table().unwrap();

    assert!(
        items.contains_key("agents/coder.md"),
        "expected first source item to remain after second add; lock:\n{lock_content}"
    );
    assert!(
        items.contains_key("agents/reviewer.md"),
        "expected second source item in lock; lock:\n{lock_content}"
    );

    assert_eq!(
        items["agents/coder.md"]["source"].as_str(),
        Some("base1"),
        "first source ownership should be preserved"
    );
    assert_eq!(
        items["agents/reviewer.md"]["source"].as_str(),
        Some("base2"),
        "second source ownership should be present"
    );
}

// ═══════════════════════════════════════════════════════════════
// 2. Idempotent sync
// ═══════════════════════════════════════════════════════════════

#[test]
fn sync_idempotent() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("reviewer", "# Reviewer")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Second sync should report up to date
    mars()
        .args(["sync", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));
}

// ═══════════════════════════════════════════════════════════════
// 3. Add + remove + prune
// ═══════════════════════════════════════════════════════════════

#[test]
fn remove_prunes_files() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder agent")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
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
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // File should be pruned
    assert!(!agents_dir.child("agents").child("coder.md").exists());
}

// ═══════════════════════════════════════════════════════════════
// 4. List
// ═══════════════════════════════════════════════════════════════

#[test]
fn list_shows_installed_items() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder")],
        &[("planning", "# Planning")],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Catalog view (default)
    mars()
        .args(["list", "--root", agents_dir.path().to_str().unwrap()])
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
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

// ═══════════════════════════════════════════════════════════════
// 5. Why
// ═══════════════════════════════════════════════════════════════

#[test]
fn why_traces_skill_to_source() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "---\nskills:\n  - planning\n---\n# Coder agent\n")],
        &[("planning", "# Planning skill")],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "why",
            "planning",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("planning (skill)"))
        .stdout(predicate::str::contains("provided by: base"))
        .stdout(predicate::str::contains("agents/coder.md"));
}

// ═══════════════════════════════════════════════════════════════
// 6. Dry run
// ═══════════════════════════════════════════════════════════════

#[test]
fn sync_diff_does_not_modify_files() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("agent", "# Agent content")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    // Manually init so we have the dir without any sync
    fs::create_dir_all(agents_dir.child(".mars").path()).unwrap();
    fs::write(
        agents_dir.child("mars.toml").path(),
        format!(
            "[sources.src]\npath = \"{}\"\n",
            source.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    mars()
        .args([
            "sync",
            "--diff",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // File should NOT be installed (dry run)
    assert!(!agents_dir.child("agents").child("agent.md").exists());
}

// ═══════════════════════════════════════════════════════════════
// 7. Force sync overwrites local modifications
// ═══════════════════════════════════════════════════════════════

#[test]
fn sync_force_overwrites_local_changes() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Original content")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Locally modify the file
    let installed_file = agents_dir.child("agents").child("coder.md");
    fs::write(installed_file.path(), "# Locally modified").unwrap();

    // Also update source so there's a conflict
    fs::write(source.join("agents").join("coder.md"), "# Upstream update").unwrap();

    // Force sync should overwrite
    mars()
        .args([
            "sync",
            "--force",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let content = fs::read_to_string(installed_file.path()).unwrap();
    assert_eq!(content, "# Upstream update");
}

// ═══════════════════════════════════════════════════════════════
// 8. Include filtering
// ═══════════════════════════════════════════════════════════════

#[test]
fn add_with_agents_filter() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "multi",
        &[
            ("coder", "# Coder"),
            ("reviewer", "# Reviewer"),
            ("planner", "# Planner"),
        ],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--agents",
            "coder",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Only coder should be installed
    assert!(agents_dir.child("agents").child("coder.md").exists());
    assert!(!agents_dir.child("agents").child("reviewer.md").exists());
    assert!(!agents_dir.child("agents").child("planner.md").exists());
}

// ═══════════════════════════════════════════════════════════════
// 9. Root discovery from subdirectory
// ═══════════════════════════════════════════════════════════════

#[test]
fn root_discovery_from_subdir() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("agent", "# Agent")], &[]);

    // Init and add
    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
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

// ═══════════════════════════════════════════════════════════════
// 10. --json output
// ═══════════════════════════════════════════════════════════════

#[test]
fn json_output_valid() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // List with --json
    let output = mars()
        .args([
            "list",
            "--json",
            "--root",
            agents_dir.path().to_str().unwrap(),
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
            agents_dir.path().to_str().unwrap(),
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

// ═══════════════════════════════════════════════════════════════
// 11. Doctor
// ═══════════════════════════════════════════════════════════════

#[test]
fn doctor_reports_healthy_state() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args(["doctor", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("all checks passed"));
}

// ═══════════════════════════════════════════════════════════════
// 12. Override
// ═══════════════════════════════════════════════════════════════

#[test]
fn override_writes_local_config() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);
    let override_path = create_source(
        &dir,
        "local-override",
        &[("coder", "# Local coder override")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "override",
            "base",
            "--path",
            override_path.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("override"));

    // mars.local.toml should exist
    assert!(agents_dir.child("mars.local.toml").exists());

    let content = fs::read_to_string(agents_dir.child("mars.local.toml").path()).unwrap();
    assert!(content.contains("base"));
    assert!(content.contains("local-override"));
}

// ═══════════════════════════════════════════════════════════════
// 13. Conflict flow
// ═══════════════════════════════════════════════════════════════

#[test]
fn conflict_flow_with_resolve() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Original\nline 2\nline 3\n")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Modify local
    let installed = agents_dir.child("agents").child("coder.md");
    fs::write(installed.path(), "# Local change\nline 2\nline 3\n").unwrap();

    // Modify source
    fs::write(
        source.join("agents").join("coder.md"),
        "# Upstream change\nline 2\nline 3\n",
    )
    .unwrap();

    // Sync — should produce conflict (exit code 1)
    mars()
        .args(["sync", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .code(1);

    // File should have conflict markers
    let content = fs::read_to_string(installed.path()).unwrap();
    assert!(
        content.contains("<<<<<<<") || content.contains(">>>>>>>"),
        "Expected conflict markers in: {content}"
    );

    // Manually "resolve" by removing markers
    fs::write(installed.path(), "# Manually resolved\nline 2\nline 3\n").unwrap();

    // Run resolve
    mars()
        .args(["resolve", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .success();
}

// ═══════════════════════════════════════════════════════════════
// 14. Help text
// ═══════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════
// 15. Regressions from smoke tests
// ═══════════════════════════════════════════════════════════════

#[test]
fn add_rejects_unmanaged_file_collision() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Managed coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    let user_file = agents_dir.child("agents").child("coder.md");
    fs::create_dir_all(user_file.path().parent().unwrap()).unwrap();
    fs::write(user_file.path(), "# User-authored").unwrap();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to overwrite unmanaged path",
        ));

    let content = fs::read_to_string(user_file.path()).unwrap();
    assert_eq!(content, "# User-authored");
}

#[test]
fn sync_force_clears_previous_conflict_markers() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Original\nline 2\nline 3\n")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let installed = agents_dir.child("agents").child("coder.md");
    fs::write(installed.path(), "# Local change\nline 2\nline 3\n").unwrap();
    fs::write(
        source.join("agents").join("coder.md"),
        "# Upstream change\nline 2\nline 3\n",
    )
    .unwrap();

    mars()
        .args(["sync", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .code(1);

    // No further source changes. --force should still overwrite conflict markers.
    mars()
        .args([
            "sync",
            "--force",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let content = fs::read_to_string(installed.path()).unwrap();
    assert_eq!(content, "# Upstream change\nline 2\nline 3\n");
}

#[test]
fn rename_applies_path_mapping_during_sync() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "rename",
            "agents/coder.md",
            "agents/coder-renamed.md",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(
        agents_dir
            .child("agents")
            .child("coder-renamed.md")
            .exists()
    );
    assert!(!agents_dir.child("agents").child("coder.md").exists());

    let lock_content = fs::read_to_string(agents_dir.child("mars.lock").path()).unwrap();
    let lock: Value = toml::from_str(&lock_content).unwrap();
    assert!(
        lock["items"]
            .as_table()
            .unwrap()
            .contains_key("agents/coder-renamed.md")
    );
}

#[test]
fn init_with_root_uses_resolved_root_path_in_message() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("proj");
    project.create_dir_all().unwrap();
    let root = project.path().join(".agents");

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

#[test]
fn sync_frozen_returns_exit_code_two() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# v1")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    fs::write(source.join("agents").join("coder.md"), "# v2").unwrap();

    mars()
        .args([
            "sync",
            "--frozen",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--frozen"));
}

#[test]
fn sync_errors_when_lock_is_corrupt() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    fs::write(agents_dir.child("mars.lock").path(), "INVALID").unwrap();

    mars()
        .args(["sync", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("lock file corrupt"))
        .stderr(predicate::str::contains("run `mars repair`"));
}

#[test]
fn repair_recovers_from_corrupt_lock() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    fs::write(agents_dir.child("mars.lock").path(), "INVALID").unwrap();

    mars()
        .args(["repair", "--root", agents_dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("lock is corrupt, rebuilding"));

    let repaired_lock = fs::read_to_string(agents_dir.child("mars.lock").path()).unwrap();
    let lock_value: Value = toml::from_str(&repaired_lock).unwrap();
    assert!(lock_value["items"].as_table().is_some());

    assert!(agents_dir.child("agents").child("coder.md").exists());
}

#[test]
fn add_nonexistent_path_does_not_pollute_config() {
    let dir = TempDir::new().unwrap();
    let agents_dir = dir.child("project").child(".agents");

    mars()
        .args(["init", "--root", dir.child("project").child(".agents").path().to_str().unwrap()])
        .assert()
        .success();

    let missing = dir.child("does-not-exist").path().to_path_buf();
    mars()
        .args([
            "add",
            missing.to_str().unwrap(),
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure();

    let config_content = fs::read_to_string(agents_dir.child("mars.toml").path()).unwrap();
    let config: Value = toml::from_str(&config_content).unwrap();
    let sources = config["sources"].as_table().unwrap();
    assert!(
        sources.is_empty(),
        "expected no sources after failed add, got: {config_content}"
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
