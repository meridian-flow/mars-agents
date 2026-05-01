mod common;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;
use toml::Value;

use common::*;

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
        items.contains_key("agent/coder"),
        "expected first source item to remain after second add; lock:\n{lock_content}"
    );
    assert!(
        items.contains_key("agent/reviewer"),
        "expected second source item in lock; lock:\n{lock_content}"
    );

    assert_eq!(
        items["agent/coder"]["source"].as_str(),
        Some("base1"),
        "first source ownership should be preserved"
    );
    assert_eq!(
        items["agent/reviewer"]["source"].as_str(),
        Some("base2"),
        "second source ownership should be present"
    );
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
