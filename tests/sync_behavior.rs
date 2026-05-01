mod common;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;

use common::*;

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
