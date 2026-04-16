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
fn add_second_source_preserves_first_source_items_in_lock() {
    let dir = TempDir::new().unwrap();
    let source_one = create_source(&dir, "base1", &[("coder", "# Coder from base1")], &[]);
    let source_two = create_source(&dir, "base2", &[("reviewer", "# Reviewer from base2")], &[]);

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
            source_one.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source_two.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock_content = fs::read_to_string(dir.child("project").child("mars.lock").path()).unwrap();
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

// ═══════════════════════════════════════════════════════════════
// 3. Add + remove + prune
// ═══════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════
// 6. Dry run
// ═══════════════════════════════════════════════════════════════

#[test]
fn sync_diff_does_not_modify_files() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("agent", "# Agent content")], &[]);

    let agents_dir = dir.child("project").child(".agents");
    // Manually init so we have the dir without any sync
    fs::create_dir_all(dir.child("project").child(".mars").path()).unwrap();
    fs::write(
        dir.child("project").child("mars.toml").path(),
        format!(
            "[dependencies.src]\npath = \"{}\"\n",
            source.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();

    mars()
        .args([
            "sync",
            "--diff",
            "--root",
            dir.child("project").path().to_str().unwrap(),
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
            dir.child("project").path().to_str().unwrap(),
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
            "--agents",
            "coder",
            "--root",
            dir.child("project").path().to_str().unwrap(),
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

// ═══════════════════════════════════════════════════════════════
// 10. --json output
// ═══════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════
// 11. Doctor
// ═══════════════════════════════════════════════════════════════

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
fn sync_json_includes_target_outcomes() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

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

    let output = mars()
        .args([
            "sync",
            "--json",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    let targets = parsed["targets"]
        .as_array()
        .expect("sync --json should include a targets array");
    assert!(!targets.is_empty());
    assert!(targets[0].get("name").is_some());
    assert!(targets[0].get("synced").is_some());
    assert!(targets[0].get("removed").is_some());
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
            "override",
            "base",
            "--path",
            override_path.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("override"));

    // mars.local.toml should exist
    assert!(dir.child("project").child("mars.local.toml").exists());

    let content = fs::read_to_string(dir.child("project").child("mars.local.toml").path()).unwrap();
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

    // Modify local in .mars/ canonical store (not .agents/ target)
    let mars_installed = dir
        .child("project")
        .child(".mars")
        .child("agents")
        .child("coder.md");
    fs::write(mars_installed.path(), "# Local change\nline 2\nline 3\n").unwrap();

    // Modify source
    fs::write(
        source.join("agents").join("coder.md"),
        "# Upstream change\nline 2\nline 3\n",
    )
    .unwrap();

    // Sync — conflicts now overwrite (source wins) with warning
    mars()
        .args([
            "sync",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(predicates::str::contains("local modifications"));

    // File in .mars/ should have upstream content (overwritten, no merge markers)
    let content = fs::read_to_string(mars_installed.path()).unwrap();
    assert_eq!(
        content, "# Upstream change\nline 2\nline 3\n",
        "Expected upstream content after overwrite, got: {content}"
    );
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
fn add_skips_unmanaged_file_collision() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Managed coder")], &[]);

    mars()
        .args([
            "init",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Place unmanaged file in .mars/ (canonical store) to trigger collision detection
    let mars_dir = dir.child("project").child(".mars");
    let user_file = mars_dir.child("agents").child("coder.md");
    fs::create_dir_all(user_file.path().parent().unwrap()).unwrap();
    fs::write(user_file.path(), "# User-authored").unwrap();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("collides with unmanaged path"));

    let content = fs::read_to_string(user_file.path()).unwrap();
    assert_eq!(content, "# User-authored");

    let lock_content =
        fs::read_to_string(dir.child("project").child("mars.lock").path()).unwrap_or_default();
    assert!(
        !lock_content.contains("agents/coder.md"),
        "collision path should not be added to lock: {lock_content}"
    );
}

#[test]
fn sync_force_overwrites_divergent_target() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[("coder", "# Original\nline 2\nline 3\n")],
        &[],
    );

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

    // Manually edit the target (.agents/) to simulate divergence
    let target_installed = dir
        .child("project")
        .child(".agents")
        .child("agents")
        .child("coder.md");
    fs::write(target_installed.path(), "# Hand-edited content\n").unwrap();

    // Normal sync should warn about divergence but preserve the edit
    mars()
        .args([
            "sync",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let content = fs::read_to_string(target_installed.path()).unwrap();
    assert_eq!(
        content, "# Hand-edited content\n",
        "Normal sync should preserve local edit"
    );

    // --force should overwrite the divergent target
    mars()
        .args([
            "sync",
            "--force",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let content = fs::read_to_string(target_installed.path()).unwrap();
    assert_eq!(
        content, "# Original\nline 2\nline 3\n",
        "--force should restore canonical content"
    );
}

#[test]
fn rename_applies_path_mapping_during_sync() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# Coder")], &[]);

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

    mars()
        .args([
            "rename",
            "agents/coder.md",
            "agents/coder-renamed.md",
            "--root",
            dir.child("project").path().to_str().unwrap(),
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

    let lock_content = fs::read_to_string(dir.child("project").child("mars.lock").path()).unwrap();
    let lock: Value = toml::from_str(&lock_content).unwrap();
    assert!(
        lock["items"]
            .as_table()
            .unwrap()
            .contains_key("agents/coder-renamed.md")
    );
}

#[test]
fn rename_skill_rewrites_agent_skill_references() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "base",
        &[(
            "coder",
            "---\nname: coder\ndescription: test agent\nskills:\n  - planning\n---\n# Coder\n",
        )],
        &[("planning", "# Planning skill")],
    );

    let project_root = dir.child("project");
    let agents_dir = project_root.child(".agents");

    mars()
        .args(["init", "--root", project_root.path().to_str().unwrap()])
        .assert()
        .success();

    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            project_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    mars()
        .args([
            "rename",
            "skills/planning",
            "skills/strategy",
            "--root",
            project_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    agents_dir
        .child("skills")
        .child("strategy")
        .child("SKILL.md")
        .assert(predicate::path::exists());
    agents_dir
        .child("skills")
        .child("planning")
        .assert(predicate::path::missing());

    let agent_content = fs::read_to_string(agents_dir.child("agents").child("coder.md").path())
        .expect("expected installed agent");
    assert!(
        agent_content.contains("- strategy"),
        "expected renamed skill ref in agent frontmatter, got:\n{agent_content}"
    );
    assert!(
        !agent_content.contains("- planning"),
        "old skill ref should be removed after rename, got:\n{agent_content}"
    );

    mars()
        .args(["doctor", "--root", project_root.path().to_str().unwrap()])
        .assert()
        .success();
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

#[test]
fn sync_frozen_returns_exit_code_two() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "base", &[("coder", "# v1")], &[]);

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

    fs::write(source.join("agents").join("coder.md"), "# v2").unwrap();

    mars()
        .args([
            "sync",
            "--frozen",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--frozen"));
}

#[test]
fn sync_errors_when_lock_is_corrupt() {
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

    fs::write(dir.child("project").child("mars.lock").path(), "INVALID").unwrap();

    mars()
        .args([
            "sync",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
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

    fs::write(dir.child("project").child("mars.lock").path(), "INVALID").unwrap();

    mars()
        .args([
            "repair",
            "--root",
            dir.child("project").path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("lock is corrupt, rebuilding"));

    let repaired_lock = fs::read_to_string(dir.child("project").child("mars.lock").path()).unwrap();
    let lock_value: Value = toml::from_str(&repaired_lock).unwrap();
    assert!(lock_value["items"].as_table().is_some());

    assert!(agents_dir.child("agents").child("coder.md").exists());
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

// ═══════════════════════════════════════════════════════════════
// Phase 4: Full pipeline integration test
// ═══════════════════════════════════════════════════════════════

#[test]
fn full_pipeline_with_local_package_and_custom_target() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();

    // 1. Init with custom target (.claude)
    mars()
        .args([
            "init",
            ".claude",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // 2. Verify mars.toml has [dependencies], not [sources], no init marker
    let config_content = fs::read_to_string(project.child("mars.toml").path()).unwrap();
    assert!(
        config_content.contains("[dependencies]"),
        "should have [dependencies]: {config_content}"
    );
    assert!(
        !config_content.contains("[sources]"),
        "should not have [sources]: {config_content}"
    );
    assert!(
        !config_content.contains("# created by mars"),
        "should not have init marker"
    );

    // 3. Verify init does not modify .gitignore.
    assert!(!project.child(".gitignore").exists());

    // 4. Verify settings.managed_root persisted
    let config: Value = toml::from_str(&config_content).unwrap();
    assert_eq!(
        config["settings"]["managed_root"].as_str(),
        Some(".claude"),
        "managed_root should be persisted"
    );

    // 5. Add [package] section and create local items
    let mut config_str = config_content.clone();
    config_str.push_str("\n[package]\nname = \"test-project\"\nversion = \"1.0.0\"\n");
    fs::write(project.child("mars.toml").path(), &config_str).unwrap();

    // Create local agent and skill
    let local_agents = project.child("agents");
    local_agents.create_dir_all().unwrap();
    fs::write(
        local_agents.child("local-agent.md").path(),
        "# Local Agent\nThis is a local agent.",
    )
    .unwrap();

    let local_skill = project.child("skills").child("local-skill");
    fs::create_dir_all(local_skill.path()).unwrap();
    fs::write(
        local_skill.child("SKILL.md").path(),
        "# Local Skill\nThis is a local skill.",
    )
    .unwrap();

    // 6. Add external dependency
    let source = create_source(
        &dir,
        "ext-source",
        &[("external-agent", "# External Agent")],
        &[],
    );
    mars()
        .args([
            "add",
            source.to_str().unwrap(),
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // 7-10. Verify items exist in target and .mars/ canonical store
    let managed = project.child(".claude");
    let local_agent_target = managed.child("agents").child("local-agent.md");
    let local_skill_target = managed.child("skills").child("local-skill");
    let external_agent = managed.child("agents").child("external-agent.md");

    assert!(
        local_agent_target.path().exists(),
        "local agent should exist"
    );
    assert!(
        local_skill_target.path().exists(),
        "local skill should exist"
    );
    assert!(
        external_agent.path().exists(),
        "external agent should exist"
    );

    // In .mars/ canonical store, local items are regular copied content
    let mars_dir = project.child(".mars");
    let mars_local_agent = mars_dir.child("agents").child("local-agent.md");
    assert!(
        !mars_local_agent
            .path()
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "local agent in .mars/ should be a regular file copy"
    );

    // In target directories, ALL items are regular file copies (D26)
    assert!(
        !local_agent_target
            .path()
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "local agent in target should be a regular file copy, not a symlink"
    );
    assert!(
        !external_agent
            .path()
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "external agent should not be a symlink"
    );

    // 11. Verify lock file has _self entries
    let lock_content = fs::read_to_string(project.child("mars.lock").path()).unwrap();
    assert!(
        lock_content.contains("[dependencies._self]"),
        "lock should have _self dependency: {lock_content}"
    );

    // 12. Verify lock file uses [dependencies.xxx] not [sources.xxx]
    assert!(
        !lock_content.contains("[sources."),
        "lock should not have [sources.]: {lock_content}"
    );

    // 13. Re-run sync — verify idempotent
    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();
    let lock_content_after_resync = fs::read_to_string(project.child("mars.lock").path()).unwrap();
    assert!(
        lock_content_after_resync.contains("[dependencies._self]"),
        "lock should retain _self dependency after re-sync: {lock_content_after_resync}"
    );
    assert!(
        lock_content_after_resync.contains("[items.\"agents/local-agent.md\"]"),
        "lock should retain local _self agent after re-sync: {lock_content_after_resync}"
    );
    assert!(
        lock_content_after_resync.contains("[items.\"skills/local-skill\"]"),
        "lock should retain local _self skill after re-sync: {lock_content_after_resync}"
    );

    // 14. Re-run init — should be idempotent
    mars()
        .args([
            "init",
            ".claude",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // 15. Verify init on package-only mars.toml succeeds
    let pkg_dir = dir.child("pkg-only");
    pkg_dir.create_dir_all().unwrap();
    fs::write(
        pkg_dir.child("mars.toml").path(),
        "[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    mars()
        .args(["init", "--root", pkg_dir.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn unlink_preserves_unrelated_config_sections() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();
    project
        .child("mars.toml")
        .write_str(
            r#"
[package]
name = "sample"
version = "0.1.0"

[dependencies.base]
url = "https://github.com/org/base.git"
version = "v1.0"
agents = ["coder"]

[settings]
targets = [".claude"]
"#,
        )
        .unwrap();

    mars()
        .args([
            "link",
            ".claude",
            "--unlink",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("settings.targets"));

    let config: Value =
        toml::from_str(&fs::read_to_string(project.child("mars.toml").path()).unwrap()).unwrap();
    assert_eq!(config["package"]["name"].as_str(), Some("sample"));
    assert_eq!(
        config["dependencies"]["base"]["url"].as_str(),
        Some("https://github.com/org/base.git")
    );
    assert_eq!(
        config["dependencies"]["base"]["version"].as_str(),
        Some("v1.0")
    );
    assert_eq!(
        config["dependencies"]["base"]["agents"][0].as_str(),
        Some("coder")
    );
    assert!(
        config["settings"]
            .as_table()
            .is_some_and(|settings| !settings.contains_key("targets"))
    );
}
