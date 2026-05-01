mod common;

use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use std::fs;
use toml::Value;

use common::*;

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
        lock_content_after_resync.contains("[items.\"agent/local-agent\"]"),
        "lock should retain local _self agent after re-sync: {lock_content_after_resync}"
    );
    assert!(
        lock_content_after_resync.contains("[items.\"skill/local-skill\"]"),
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
fn sync_prefers_mars_src_local_items_over_repo_root() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();

    mars()
        .args(["init", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    fs::write(
        project.child("mars.toml").path(),
        "[dependencies]\n\n[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();

    let legacy_skill = project.child("skills").child("planning");
    legacy_skill.create_dir_all().unwrap();
    legacy_skill
        .child("SKILL.md")
        .write_str("# Legacy")
        .unwrap();

    let preferred_skill = project.child(".mars-src").child("skills").child("planning");
    preferred_skill.create_dir_all().unwrap();
    preferred_skill
        .child("SKILL.md")
        .write_str("# Preferred")
        .unwrap();

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("defined in both"))
        .stderr(predicate::str::contains(".mars-src"));

    assert_eq!(
        fs::read_to_string(
            project
                .child(".agents")
                .child("skills")
                .child("planning")
                .child("SKILL.md")
                .path()
        )
        .unwrap(),
        "# Preferred"
    );
}

#[test]
fn adopt_moves_skill_into_mars_src_and_syncs_targets() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();

    mars()
        .args([
            "init",
            ".claude",
            "--link",
            ".agents",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let unmanaged_skill = project
        .child(".claude")
        .child("skills")
        .child("local-skill");
    unmanaged_skill.create_dir_all().unwrap();
    unmanaged_skill
        .child("SKILL.md")
        .write_str("# Local skill")
        .unwrap();

    mars()
        .args([
            "adopt",
            unmanaged_skill.path().to_str().unwrap(),
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("adopted skill `local-skill`"));

    let local_source_skill = project
        .child(".mars-src")
        .child("skills")
        .child("local-skill")
        .child("SKILL.md");
    assert!(local_source_skill.exists());
    assert_eq!(
        fs::read_to_string(local_source_skill.path()).unwrap(),
        "# Local skill"
    );

    assert!(
        project
            .child(".claude")
            .child("skills")
            .child("local-skill")
            .child("SKILL.md")
            .exists()
    );
    assert!(
        project
            .child(".agents")
            .child("skills")
            .child("local-skill")
            .child("SKILL.md")
            .exists()
    );
    assert!(
        project
            .child(".mars")
            .child("skills")
            .child("local-skill")
            .child("SKILL.md")
            .exists()
    );

    let lock_content = fs::read_to_string(project.child("mars.lock").path()).unwrap();
    assert!(lock_content.contains("[dependencies._self]"));
    assert!(lock_content.contains("[items.\"skill/local-skill\"]"));
}

#[test]
fn sync_reads_mars_src_local_items_without_package_section() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();

    mars()
        .args([
            "init",
            ".claude",
            "--link",
            ".agents",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let local_skill = project
        .child(".mars-src")
        .child("skills")
        .child("local-only");
    local_skill.create_dir_all().unwrap();
    local_skill
        .child("SKILL.md")
        .write_str("# Local only")
        .unwrap();

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    assert!(
        project
            .child(".claude")
            .child("skills")
            .child("local-only")
            .child("SKILL.md")
            .exists()
    );
    assert!(
        project
            .child(".agents")
            .child("skills")
            .child("local-only")
            .child("SKILL.md")
            .exists()
    );
    let lock_content = fs::read_to_string(project.child("mars.lock").path()).unwrap();
    assert!(lock_content.contains("[dependencies._self]"));
    assert!(lock_content.contains("[items.\"skill/local-only\"]"));
}

#[test]
fn sync_ignores_repo_root_local_items_without_package_section() {
    let dir = TempDir::new().unwrap();
    let project = dir.child("project");
    project.create_dir_all().unwrap();

    mars()
        .args(["init", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    let legacy_skill = project.child("skills").child("legacy-only");
    legacy_skill.create_dir_all().unwrap();
    legacy_skill
        .child("SKILL.md")
        .write_str("# Legacy only")
        .unwrap();

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));

    assert!(
        !project
            .child(".agents")
            .child("skills")
            .child("legacy-only")
            .child("SKILL.md")
            .exists()
    );
}
