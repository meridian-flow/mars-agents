mod common;

use assert_fs::TempDir;
use assert_fs::prelude::*;
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
fn list_and_export_include_bootstrap_docs() {
    let dir = TempDir::new().unwrap();
    let source = create_source(&dir, "src", &[("coder", "# Coder")], &[]);
    let bootstrap_dir = source.join("bootstrap/global-auth");
    fs::create_dir_all(&bootstrap_dir).unwrap();
    fs::write(
        bootstrap_dir.join("BOOTSTRAP.md"),
        "---\nname: global-auth\ndescription: auth setup\n---\n# Auth",
    )
    .unwrap();

    let project = dir.child("proj");
    project.create_dir_all().unwrap();
    project
        .child("mars.toml")
        .write_str(&format!(
            "[dependencies]\nsrc = {{ path = \"{}\" }}\n",
            source.display()
        ))
        .unwrap();

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    let list_output = mars()
        .args(["--json", "list", "--root", project.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(list_output.status.success(), "mars list should succeed");
    let list_stdout = String::from_utf8(list_output.stdout).unwrap();
    let list_json: serde_json::Value = serde_json::from_str(&list_stdout).expect("valid list JSON");
    assert_eq!(
        list_json["bootstrap"][0]["name"].as_str(),
        Some("global-auth"),
        "expected bootstrap doc in list output: {list_stdout}"
    );

    let export_output = mars()
        .args(["export", "--root", project.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(export_output.status.success(), "mars export should succeed");
    let export_stdout = String::from_utf8(export_output.stdout).unwrap();
    let export_json: serde_json::Value =
        serde_json::from_str(&export_stdout).expect("valid export JSON");
    let items = export_json["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "bootstrap-doc" && item["name"] == "global-auth"),
        "expected bootstrap-doc in export output: {export_stdout}"
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

#[test]
fn validate_json_reports_skill_legacy_field_warning() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: reader\ndescription: reads things\n---\n# Reader";
    let skill_content = "---\nname: legacy\ndescription: legacy skill\nallow_implicit_invocation: false\n---\n# Legacy";
    let project = setup_synced_project(
        &dir,
        "proj",
        "src",
        &[("reader", agent_content)],
        &[("legacy", skill_content)],
    );

    let output = mars()
        .args(["validate", "--json", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "warning-only validate should succeed"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|diag| {
            diag["code"] == "skill-schema-warning"
                && diag["message"].as_str().is_some_and(|message| {
                    message.contains("deprecated `allow_implicit_invocation`")
                })
        }),
        "expected legacy skill deprecation warning: {stdout}"
    );
}

#[test]
fn list_json_includes_skill_variant_availability() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "src",
        &[("reader", "# Reader")],
        &[(
            "planning",
            "---\nname: planning\ndescription: plan helper\n---\n# Planning",
        )],
    );
    fs::create_dir_all(source.join("skills/planning/variants/claude")).unwrap();
    fs::create_dir_all(source.join("skills/planning/variants/codex/gpt-5")).unwrap();
    fs::write(
        source.join("skills/planning/variants/claude/SKILL.md"),
        "# Claude",
    )
    .unwrap();
    fs::write(
        source.join("skills/planning/variants/codex/gpt-5/SKILL.md"),
        "# Codex",
    )
    .unwrap();

    let project = dir.child("proj");
    project.create_dir_all().unwrap();
    project
        .child("mars.toml")
        .write_str(&format!(
            "[dependencies]\nsrc = {{ path = \"{}\" }}\n",
            source.display()
        ))
        .unwrap();
    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    let output = mars()
        .args(["--json", "list", "--root", project.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "mars list should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let skills = json["skills"].as_array().unwrap();
    let planning = skills
        .iter()
        .find(|entry| entry["name"] == "planning")
        .expect("planning skill should be listed");
    assert_eq!(planning["variants"].as_str(), Some("claude, codex"));
}

#[test]
fn validate_json_reports_malformed_skill_frontmatter_error() {
    let dir = TempDir::new().unwrap();
    let agent_content = "---\nname: reader\ndescription: reads things\n---\n# Reader";
    let malformed_skill =
        "---\nname: planning\ndescription: broken skill\nmetadata: [\n---\n# Broken\n";
    let project = setup_synced_project(
        &dir,
        "proj",
        "src",
        &[("reader", agent_content)],
        &[("planning", malformed_skill)],
    );

    let output = mars()
        .args(["validate", "--json", "--root", project.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "malformed skill frontmatter should make validate fail"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        json["clean"].as_bool(),
        Some(false),
        "expected clean=false for malformed frontmatter: {stdout}"
    );
    assert!(
        json["error_count"].as_u64().unwrap_or(0) > 0,
        "expected nonzero error_count for malformed frontmatter: {stdout}"
    );
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|diag| {
            diag["code"] == "skill-schema-error"
                && diag["message"].as_str().is_some_and(|message| {
                    message.contains("skill `planning`")
                        && message.contains("skill frontmatter is malformed")
                })
        }),
        "expected malformed skill frontmatter error: {stdout}"
    );
}

#[test]
#[ignore = "known gap: SV-W1 unknown harness variants do not yet surface validate warnings"]
fn validate_json_reports_unknown_skill_variant_harness_warning() {
    let dir = TempDir::new().unwrap();
    let source = create_source(
        &dir,
        "src",
        &[("reader", "# Reader")],
        &[(
            "planning",
            "---\nname: planning\ndescription: plan helper\n---\n# Planning",
        )],
    );
    fs::create_dir_all(source.join("skills/planning/variants/mystery-harness")).unwrap();
    fs::write(
        source.join("skills/planning/variants/mystery-harness/SKILL.md"),
        "# Mystery Harness",
    )
    .unwrap();

    let project = dir.child("proj");
    project.create_dir_all().unwrap();
    project
        .child("mars.toml")
        .write_str(&format!(
            "[dependencies]\nsrc = {{ path = \"{}\" }}\n",
            source.display()
        ))
        .unwrap();

    let output = mars()
        .args([
            "validate",
            "--json",
            "--root",
            project.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "unknown harness variant should warn without failing validate"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|diag| {
            diag["level"] == "warning"
                && diag["message"].as_str().is_some_and(|message| {
                    message.contains("mystery-harness")
                        && message.contains("variant")
                        && message.contains("planning")
                })
        }),
        "expected validate warning for unknown skill variant harness: {stdout}"
    );
}
