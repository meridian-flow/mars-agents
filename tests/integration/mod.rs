// Integration tests for mars-agents.
//
// These tests exercise the CLI binary end-to-end using assert_cmd.
// Each test creates a temp directory with synthetic source content
// and runs mars commands against it.

use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;
use std::fs;

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
        .args(["init", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized"));

    let agents_dir = dir.child(".agents");
    assert!(agents_dir.child("agents.toml").exists());
    assert!(agents_dir.child(".mars").exists());
    assert!(agents_dir.child(".gitignore").exists());
}

#[test]
fn init_twice_fails() {
    let dir = TempDir::new().unwrap();

    mars()
        .args(["init", dir.path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args(["init", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
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
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    assert!(agents_dir.child("skills").child("planning").child("SKILL.md").exists());

    // Verify lock file exists
    assert!(agents_dir.child("agents.lock").exists());
}

// ═══════════════════════════════════════════════════════════════
// 2. Idempotent sync
// ═══════════════════════════════════════════════════════════════

#[test]
fn sync_idempotent() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "src",
        &[("reviewer", "# Reviewer")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
        .args([
            "sync",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
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
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder agent")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
            "list",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("coder"))
        .stdout(predicate::str::contains("planning"))
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
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    let source = create_source(
        &dir,
        "src",
        &[("agent", "# Agent content")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    // Manually init so we have the dir without any sync
    fs::create_dir_all(agents_dir.child(".mars").path()).unwrap();
    fs::write(
        agents_dir.child("agents.toml").path(),
        format!("[sources.src]\npath = \"{}\"\n", source.display().to_string().replace('\\', "/")),
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
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Original content")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    fs::write(
        source.join("agents").join("coder.md"),
        "# Upstream update",
    )
    .unwrap();

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
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    let source = create_source(
        &dir,
        "src",
        &[("agent", "# Agent")],
        &[],
    );

    // Init and add
    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
    // Should be valid JSON
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
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
            "doctor",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
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
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Coder")],
        &[],
    );
    let override_path = create_source(
        &dir,
        "local-override",
        &[("coder", "# Local coder override")],
        &[],
    );

    let agents_dir = dir.child("project").child(".agents");
    mars()
        .args(["init", dir.child("project").path().to_str().unwrap()])
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

    // agents.local.toml should exist
    assert!(agents_dir.child("agents.local.toml").exists());

    let content = fs::read_to_string(agents_dir.child("agents.local.toml").path()).unwrap();
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
        .args(["init", dir.child("project").path().to_str().unwrap()])
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
        .args([
            "sync",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
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
        .args([
            "resolve",
            "--root",
            agents_dir.path().to_str().unwrap(),
        ])
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
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("outdated"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("why"))
        .stdout(predicate::str::contains("rename"))
        .stdout(predicate::str::contains("resolve"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("repair"));
}
