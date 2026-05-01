mod common;

use httpmock::prelude::*;
use serde_json::{Value, json};
use serial_test::serial;
use std::fs;

use common::*;

#[test]
#[serial]
fn scenario_f_add_sync_force_and_resolve_dependency_alias() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });

    let (temp, project_root) = setup_project(&server);
    let source_root = write_local_source_with_model_alias(
        temp.path(),
        "alias-source-force-sync",
        "test-alias",
        "openai/gpt-5",
    );

    let mut add_cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    add_cmd.arg("add").arg(source_root.as_os_str());
    add_cmd.assert().success();

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["sync", "--force"]);
    cmd.assert().success();

    let mut resolve_cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    resolve_cmd.args(["--json", "models", "resolve", "test-alias"]);
    let resolve_output = resolve_cmd.assert().success().get_output().clone();
    let resolve_json: Value =
        serde_json::from_slice(&resolve_output.stdout).expect("resolve --json should return JSON");
    assert_eq!(
        resolve_json["resolved_model"].as_str(),
        Some("openai/gpt-5"),
        "expected dependency alias to resolve to pinned model"
    );

    let cache = read_cache_json(&project_root);
    assert!(
        cache["models"]
            .as_array()
            .expect("cache.models should be an array")
            .len()
            >= 2,
        "expected sync to populate models cache"
    );
    assert!(
        cache["fetched_at"].as_str().is_some(),
        "expected fetched_at to be set after sync"
    );
    assert!(
        models_merged_path(&project_root).exists(),
        "expected models-merged.json to be written during sync"
    );
    let merged: Value = serde_json::from_str(
        &fs::read_to_string(models_merged_path(&project_root))
            .expect("failed to read models-merged.json"),
    )
    .expect("failed to parse models-merged.json");
    assert!(
        merged.get("test-alias").is_some(),
        "expected dependency alias in models-merged.json"
    );
    assert_eq!(
        mock.hits(),
        1,
        "expected add+sync+resolve flow to fetch models catalog once"
    );
}

#[test]
#[serial]
fn scenario_h_add_immediately_resolve_alias_without_explicit_sync() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });

    let (temp, project_root) = setup_project(&server);
    let source_root = write_local_source_with_model_alias(
        temp.path(),
        "alias-source-immediate",
        "test-alias-immediate",
        "openai/gpt-5",
    );

    let mut add_cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    add_cmd.arg("add").arg(source_root.as_os_str());
    add_cmd.assert().success();

    let mut resolve_cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    resolve_cmd.args(["models", "resolve", "test-alias-immediate"]);
    let resolve_output = resolve_cmd.assert().success().get_output().clone();
    let resolve_stdout =
        String::from_utf8(resolve_output.stdout).expect("resolve stdout should be utf-8");
    assert!(
        resolve_stdout.contains("openai/gpt-5"),
        "expected resolved pinned model in resolve output:\n{resolve_stdout}"
    );
    assert!(
        models_merged_path(&project_root).exists(),
        "expected models-merged.json after add-triggered sync"
    );
    assert_eq!(
        mock.hits(),
        1,
        "expected add+immediate resolve online flow to fetch models catalog once"
    );
}

#[test]
#[serial]
fn resolve_alias_prefix_exits_zero() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "resolve", "opus-4-6"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("resolve --json should return JSON");

    assert_eq!(stdout["source"].as_str(), Some("alias_prefix"));
    assert_eq!(stdout["name"].as_str(), Some("opus-4-6"));
    assert_eq!(stdout["resolved_model"].as_str(), Some("claude-opus-4-6"));
}

#[test]
#[serial]
fn resolve_unknown_exits_zero_with_passthrough() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "resolve", "unknown-xyz"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("resolve --json should return JSON");

    assert_eq!(stdout["source"].as_str(), Some("passthrough"));
    assert_eq!(stdout["model_id"].as_str(), Some("unknown-xyz"));
    assert_eq!(stdout["resolved_model"].as_str(), Some("unknown-xyz"));
    assert_eq!(stdout["provider"], Value::Null);
    assert_eq!(stdout["harness"], Value::Null);
    assert_eq!(stdout["harness_source"].as_str(), Some("unavailable"));
    assert_eq!(stdout["harness_candidates"], json!([]));
    assert!(stdout["availability"].is_string());
    assert!(stdout["availability_source"].is_string());
    assert!(
        stdout["runnable_paths"].is_array(),
        "passthrough JSON should include availability runnable paths"
    );
    assert!(
        stdout["warning"]
            .as_str()
            .expect("passthrough warning should be present")
            .contains("passing through to harness")
    );
}

#[test]
#[serial]
fn models_list_visibility_include_does_not_add_catalog_rows() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    fs::write(
        project_root.join("mars.toml"),
        r#"[settings]

[settings.model_visibility]
include = ["catalog-only-*"]
"#,
    )
    .expect("failed to write mars.toml with model visibility");
    write_cache(
        &project_root,
        vec![json!({
            "id": "catalog-only-model",
            "provider": "OpenAI",
            "release_date": "2026-01-01"
        })],
        &fresh_fetched_at(),
    );

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "list"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("models list --json should return JSON");

    let aliases = stdout["aliases"]
        .as_array()
        .expect("models list JSON should include aliases");
    assert!(
        aliases.is_empty(),
        "default models list should not expand visibility includes into catalog rows"
    );
}

#[test]
#[serial]
fn resolve_prefix_no_match_exits_zero_with_passthrough() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "resolve", "opus-9-9"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("resolve --json should return JSON");

    assert_eq!(stdout["source"].as_str(), Some("passthrough"));
    assert_eq!(stdout["resolved_model"].as_str(), Some("opus-9-9"));
}

#[test]
#[serial]
fn resolve_passthrough_pattern_guesses_harness() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "resolve", "claude-brand-new"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("resolve --json should return JSON");

    let expected_harness = ["claude", "opencode", "gemini"]
        .iter()
        .find(|bin| which::which(bin).is_ok())
        .map(|bin| (*bin).to_string());
    let expected_source = if expected_harness.is_some() {
        "pattern_guess"
    } else {
        "unavailable"
    };

    assert_eq!(stdout["source"].as_str(), Some("passthrough"));
    assert_eq!(stdout["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        stdout["harness_candidates"],
        json!(["claude", "opencode", "gemini"])
    );
    assert_eq!(stdout["harness_source"].as_str(), Some(expected_source));
    assert_eq!(stdout["harness"].as_str(), expected_harness.as_deref());
}

#[test]
#[serial]
fn resolve_passthrough_unrecognized_pattern_harness_null() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "resolve", "xyz-unknown"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("resolve --json should return JSON");

    assert_eq!(stdout["source"].as_str(), Some("passthrough"));
    assert_eq!(stdout["provider"], Value::Null);
    assert_eq!(stdout["harness"], Value::Null);
    assert_eq!(stdout["harness_source"].as_str(), Some("unavailable"));
    assert_eq!(stdout["harness_candidates"], json!([]));
}

#[test]
#[serial]
fn resolve_unknown_with_no_refresh_without_cache_is_non_zero() {
    let server = MockServer::start();
    let (temp, project_root) = setup_project(&server);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["models", "resolve", "unknown-xyz", "--no-refresh-models"]);

    let output = cmd.assert().code(3).get_output().clone();
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");
    assert!(
        stderr.contains("--no-refresh-models"),
        "expected no-refresh cache error, stderr:\n{stderr}"
    );
}
