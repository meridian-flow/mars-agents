mod common;

use assert_fs::TempDir;
use std::fs;

use common::*;

#[test]
fn validate_exits_zero_on_clean_project() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: coder\ndescription: a coding agent\n---\n# Coder";
    let project = setup_synced_project(&dir, "proj", "src", &[("coder", agent_content)], &[]);

    mars()
        .args(["validate", "--root", project.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn validate_json_outputs_clean_true_on_success() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: reader\ndescription: reads things\n---\n# Reader";
    let project = setup_synced_project(&dir, "proj", "src", &[("reader", agent_content)], &[]);

    let output = mars()
        .args(["validate", "--json", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(
        json["clean"].as_bool(),
        Some(true),
        "expected clean=true in JSON output: {stdout}"
    );
    assert!(
        json["diagnostics"].is_array(),
        "expected diagnostics array in JSON output: {stdout}"
    );
    assert_eq!(
        json["error_count"].as_u64(),
        Some(0),
        "expected zero errors: {stdout}"
    );
}

#[test]
fn validate_strict_clean_project_still_passes() {
    // --strict on a project with zero warnings should still exit 0.
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: planner\ndescription: plans\n---\n# Planner";
    let project = setup_synced_project(&dir, "proj", "src", &[("planner", agent_content)], &[]);

    mars()
        .args(["validate", "--strict", "--root", project.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn validate_strict_with_override_warning() {
    // override-missing-dep is a Warning diagnostic.
    // --strict should escalate it to an error (exit 1).
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: agent\ndescription: an agent\n---\n# Agent";
    let project = setup_synced_project(&dir, "proj", "src", &[("agent", agent_content)], &[]);

    // Add mars.local.toml with an override pointing to a non-existent dep.
    // This produces an override-missing-dep Warning diagnostic.
    let local_toml = "[overrides.nonexistent-dep]\npath = \"/does/not/exist\"\n";
    fs::write(project.join("mars.local.toml"), local_toml).unwrap();

    // Normal validate exits 0 (warning doesn't fail)
    mars()
        .args(["validate", "--root", project.to_str().unwrap()])
        .assert()
        .success();

    // --strict exits 1 (warning escalated to error)
    mars()
        .args(["validate", "--strict", "--root", project.to_str().unwrap()])
        .assert()
        .failure();
}

#[test]
fn export_exits_zero_and_outputs_json() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: writer\ndescription: writes things\n---\n# Writer";
    let project = setup_synced_project(&dir, "proj", "src", &[("writer", agent_content)], &[]);

    let output = mars()
        .args(["export", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "export should exit 0");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");

    assert_eq!(
        json["schema_version"].as_u64(),
        Some(1),
        "expected schema_version=1: {stdout}"
    );
    assert!(
        json["status"].is_string(),
        "expected status field: {stdout}"
    );
    assert!(json["items"].is_array(), "expected items array: {stdout}");
    assert!(
        json["outputs"].is_array(),
        "expected outputs array: {stdout}"
    );
    assert!(
        json["diagnostics"].is_array(),
        "expected diagnostics array: {stdout}"
    );
    assert!(
        json["dependencies"].is_array(),
        "expected dependencies array: {stdout}"
    );
}

#[test]
fn export_complete_status_on_clean_project() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: builder\ndescription: builds things\n---\n# Builder";
    let skill_content = "---\nname: make\ndescription: make helper\n---\n# Make";
    let project = setup_synced_project(
        &dir,
        "proj",
        "src",
        &[("builder", agent_content)],
        &[("make", skill_content)],
    );

    let output = mars()
        .args(["export", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        json["status"].as_str(),
        Some("complete"),
        "expected complete status: {stdout}"
    );
}

#[test]
fn export_no_file_bodies_in_output() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: secret-agent\ndescription: secret\n---\n# TOP SECRET CONTENT";
    let project =
        setup_synced_project(&dir, "proj", "src", &[("secret-agent", agent_content)], &[]);

    let output = mars()
        .args(["export", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("TOP SECRET CONTENT"),
        "export must not include file body content: {stdout}"
    );
}

#[test]
fn validate_json_strict_escalates_warnings_in_output() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: alpha\ndescription: agent\n---\n# Alpha";
    let project = setup_synced_project(&dir, "proj", "src", &[("alpha", agent_content)], &[]);

    // Add override-missing-dep warning via mars.local.toml.
    let local_toml = "[overrides.ghost-dep]\npath = \"/does/not/exist\"\n";
    fs::write(project.join("mars.local.toml"), local_toml).unwrap();

    let output = mars()
        .args([
            "validate",
            "--strict",
            "--json",
            "--root",
            project.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Exit code should be non-zero (warning escalated to error)
    assert!(
        !output.status.success(),
        "strict mode should fail on missing-skill warning"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        json["clean"].as_bool(),
        Some(false),
        "expected clean=false in strict mode: {stdout}"
    );
    assert!(
        json["error_count"].as_u64().unwrap_or(0) > 0,
        "expected nonzero error_count in strict mode: {stdout}"
    );

    // All diagnostics at 'warning' level in the pipeline should appear as 'error' in output
    if let Some(diags) = json["diagnostics"].as_array() {
        for diag in diags {
            let level = diag["level"].as_str().unwrap_or("");
            assert_ne!(
                level, "warning",
                "strict mode should escalate warnings to errors: {stdout}"
            );
        }
    }
}
