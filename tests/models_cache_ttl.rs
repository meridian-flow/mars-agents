use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin;
use httpmock::prelude::*;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::{TempDir, tempdir};

const API_PATH: &str = "/api.json";

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn fresh_fetched_at() -> String {
    now_unix_secs().saturating_sub(60).to_string()
}

fn stale_fetched_at() -> String {
    now_unix_secs().saturating_sub(25 * 3600).to_string()
}

fn sample_catalog_json() -> Value {
    json!({
        "anthropic": {
            "models": {
                "claude-opus-4-6": {
                    "id": "claude-opus-4-6",
                    "name": "Claude Opus 4.6",
                    "release_date": "2026-02-05",
                    "limit": {
                        "context": 1000000,
                        "output": 128000
                    }
                }
            }
        },
        "openai": {
            "models": {
                "gpt-5": {
                    "id": "gpt-5",
                    "name": "GPT-5",
                    "release_date": "2025-06-01",
                    "limit": {
                        "context": 400000,
                        "output": 128000
                    }
                }
            }
        }
    })
}

fn sample_cached_models() -> Vec<Value> {
    vec![
        json!({
            "id": "claude-opus-4-6",
            "provider": "Anthropic",
            "release_date": "2026-02-05"
        }),
        json!({
            "id": "gpt-5",
            "provider": "OpenAI",
            "release_date": "2025-06-01"
        }),
    ]
}

fn cache_path(project_root: &Path) -> PathBuf {
    project_root.join(".mars").join("models-cache.json")
}

fn models_merged_path(project_root: &Path) -> PathBuf {
    project_root.join(".mars").join("models-merged.json")
}

fn write_local_source_with_model_alias(
    temp_root: &Path,
    source_dir_name: &str,
    alias_name: &str,
    model_id: &str,
) -> PathBuf {
    let source_root = temp_root.join(source_dir_name);
    let agents_dir = source_root.join("agents");
    fs::create_dir_all(&agents_dir).expect("failed to create local source agents dir");
    fs::write(
        agents_dir.join("fixture.md"),
        "# Fixture agent for models-cache tests\n",
    )
    .expect("failed to write local source fixture agent");

    let source_manifest = format!(
        r#"[package]
name = "{source_dir_name}"
version = "0.1.0"

[models."{alias_name}"]
harness = "codex"
model = "{model_id}"
description = "fixture alias for models cache tests"
"#
    );
    fs::write(source_root.join("mars.toml"), source_manifest)
        .expect("failed to write local source mars.toml");
    source_root
}

fn resolved_model_ids_from_models_list_json(stdout: &[u8]) -> BTreeSet<String> {
    let payload: Value =
        serde_json::from_slice(stdout).expect("models list --json must be valid JSON");
    payload["aliases"]
        .as_array()
        .expect("models list JSON should include aliases array")
        .iter()
        .filter_map(|alias| {
            alias["resolved_model"]
                .as_str()
                .or_else(|| alias["model_id"].as_str())
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn write_cache(project_root: &Path, models: Vec<Value>, fetched_at: &str) {
    let mars_dir = project_root.join(".mars");
    fs::create_dir_all(&mars_dir).expect("failed to create .mars directory");
    let cache = json!({
        "models": models,
        "fetched_at": fetched_at,
    });
    fs::write(
        cache_path(project_root),
        serde_json::to_vec_pretty(&cache).expect("failed to serialize cache fixture"),
    )
    .expect("failed to write cache fixture");
}

fn read_cache_json(project_root: &Path) -> Value {
    let raw = fs::read_to_string(cache_path(project_root)).expect("failed to read cache file");
    serde_json::from_str(&raw).expect("failed to parse cache file JSON")
}

fn read_cache_raw(project_root: &Path) -> String {
    fs::read_to_string(cache_path(project_root)).expect("failed to read cache file")
}

fn configure_assert_cmd(cmd: &mut Command, temp_root: &Path, api_url: &str) {
    let home = temp_root.join("home");
    let xdg_config = temp_root.join("xdg-config");
    let xdg_cache = temp_root.join("xdg-cache");
    let xdg_data = temp_root.join("xdg-data");

    for dir in [&home, &xdg_config, &xdg_cache, &xdg_data] {
        fs::create_dir_all(dir).expect("failed to create isolated env directory");
    }

    cmd.env("MARS_MODELS_API_URL", api_url)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("NO_COLOR", "1")
        .env_remove("MARS_CACHE_DIR")
        .env_remove("MARS_OFFLINE");
}

fn configure_std_cmd(cmd: &mut StdCommand, temp_root: &Path, api_url: &str) {
    let home = temp_root.join("home");
    let xdg_config = temp_root.join("xdg-config");
    let xdg_cache = temp_root.join("xdg-cache");
    let xdg_data = temp_root.join("xdg-data");

    for dir in [&home, &xdg_config, &xdg_cache, &xdg_data] {
        fs::create_dir_all(dir).expect("failed to create isolated env directory");
    }

    cmd.env("MARS_MODELS_API_URL", api_url)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("NO_COLOR", "1")
        .env_remove("MARS_CACHE_DIR")
        .env_remove("MARS_OFFLINE");
}

fn mars_cmd(project_root: &Path, temp_root: &Path, api_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("mars").expect("failed to locate mars test binary");
    configure_assert_cmd(&mut cmd, temp_root, api_url);
    cmd.arg("--root").arg(project_root);
    cmd
}

fn init_project(project_root: &Path, temp_root: &Path, api_url: &str) {
    fs::create_dir_all(project_root).expect("failed to create project root");

    let mut cmd = mars_cmd(project_root, temp_root, api_url);
    cmd.arg("init");
    cmd.assert().success();
}

fn setup_project(server: &MockServer) -> (TempDir, PathBuf) {
    let temp = tempdir().expect("failed to create temp dir");
    let project_root = temp.path().join("project");
    init_project(&project_root, temp.path(), &server.url(API_PATH));
    (temp, project_root)
}

#[test]
#[serial]
fn scenario_a_cold_cache_refreshes_on_models_list() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });

    let (temp, project_root) = setup_project(&server);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["--json", "models", "list"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("models list --json should return JSON");

    assert!(
        stdout["aliases"].is_array(),
        "expected aliases array in JSON"
    );

    let cache = read_cache_json(&project_root);
    assert!(
        cache["models"]
            .as_array()
            .expect("cache.models should be an array")
            .len()
            >= 2,
        "expected non-empty models cache"
    );
    assert!(
        cache["fetched_at"].as_str().is_some(),
        "expected fetched_at timestamp"
    );
    assert_eq!(mock.hits(), 1, "expected one fetch for cold cache");
}

#[test]
#[serial]
fn scenario_b_fresh_cache_skips_fetch() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(500).body("server error");
    });

    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &fresh_fetched_at());
    let before = read_cache_raw(&project_root);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["models", "list", "--all"]);
    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    assert!(
        stdout.contains("gpt-5"),
        "expected cached model id in list output:\n{stdout}"
    );

    let after = read_cache_raw(&project_root);
    assert_eq!(before, after, "fresh cache should stay unchanged");
    assert_eq!(mock.hits(), 0, "fresh cache should skip network fetch");
}

#[test]
#[serial]
fn scenario_c_stale_cache_falls_back_on_fetch_failure() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(500).body("server error");
    });

    let (temp, project_root) = setup_project(&server);
    write_cache(&project_root, sample_cached_models(), &stale_fetched_at());
    let before = read_cache_raw(&project_root);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["models", "list", "--all"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");
    assert!(
        stderr.contains("models cache refresh failed") && stderr.contains("stale cache"),
        "expected stale cache warning, stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("gpt-5"),
        "expected cached model id in list output:\n{stdout}"
    );

    let after = read_cache_raw(&project_root);
    assert_eq!(before, after, "stale fallback must not rewrite cache");
    assert_eq!(mock.hits(), 1, "stale cache should attempt one refresh");
}

#[test]
#[serial]
fn scenario_d_empty_cache_offline_errors_cleanly() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });
    let (temp, project_root) = setup_project(&server);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.env("MARS_OFFLINE", "1");
    cmd.args(["models", "resolve", "opus"]);

    let output = cmd.assert().code(3).get_output().clone();
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");

    assert!(
        stderr.contains("MARS_OFFLINE"),
        "expected MARS_OFFLINE mention in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("mars models refresh"),
        "expected refresh hint in stderr:\n{stderr}"
    );
    assert_eq!(
        mock.hits(),
        0,
        "offline resolve should not hit models endpoint"
    );
    assert!(
        !cache_path(&project_root).exists(),
        "offline resolve with empty cache should not create cache file"
    );
}

#[test]
#[serial]
fn scenario_e_no_refresh_models_flag_matches_offline_behavior() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });
    let (temp, project_root) = setup_project(&server);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["models", "resolve", "opus", "--no-refresh-models"]);

    let output = cmd.assert().code(3).get_output().clone();
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf-8");

    assert!(
        stderr.contains("--no-refresh-models"),
        "expected --no-refresh-models mention in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("mars models refresh"),
        "expected refresh hint in stderr:\n{stderr}"
    );
    assert_eq!(
        mock.hits(),
        0,
        "no-refresh flag should not hit models endpoint when cache is missing"
    );
    assert!(
        !cache_path(&project_root).exists(),
        "--no-refresh-models with empty cache should not create cache file"
    );
}

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
fn scenario_g_offline_sync_succeeds_without_cache_and_emits_diag() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });
    let (temp, project_root) = setup_project(&server);

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.env("MARS_OFFLINE", "1");
    cmd.args(["--json", "sync", "--force"]);

    let output = cmd.assert().success().get_output().clone();
    let stdout: Value =
        serde_json::from_slice(&output.stdout).expect("sync --json should return JSON");

    let diagnostics = stdout["diagnostics"]
        .as_array()
        .expect("sync JSON should include diagnostics array");
    assert!(
        diagnostics
            .iter()
            .any(|d| d["code"].as_str() == Some("models-cache-refresh")),
        "expected models-cache-refresh warning in diagnostics"
    );
    assert!(
        !cache_path(&project_root).exists(),
        "offline sync with empty cache should not create cache file"
    );
    assert!(
        models_merged_path(&project_root).exists(),
        "offline sync should still write models-merged.json"
    );
    assert_eq!(
        mock.hits(),
        0,
        "offline sync should not hit models endpoint"
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
fn scenario_i_concurrent_processes_fetch_once() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200)
            .delay(Duration::from_millis(500))
            .json_body(sample_catalog_json());
    });

    let (temp, project_root) = setup_project(&server);
    let bin_path = cargo_bin("mars");
    let api_url = server.url(API_PATH);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let bin_path = bin_path.clone();
            let env_root = temp.path().to_path_buf();
            let root = project_root.clone();
            let api_url = api_url.clone();

            thread::spawn(move || {
                let mut cmd = StdCommand::new(&bin_path);
                configure_std_cmd(&mut cmd, &env_root, &api_url);
                cmd.arg("--root")
                    .arg(root)
                    .arg("--json")
                    .arg("models")
                    .arg("list")
                    .output()
                    .expect("failed to execute concurrent mars models list")
            })
        })
        .collect();

    let outputs: Vec<Output> = handles
        .into_iter()
        .map(|h| h.join().expect("concurrent worker thread panicked"))
        .collect();

    let expected_catalog_ids: BTreeSet<String> =
        vec!["claude-opus-4-6".to_string(), "gpt-5".to_string()]
            .into_iter()
            .collect();
    let mut baseline_model_ids: Option<BTreeSet<String>> = None;

    for output in outputs {
        assert!(
            output.status.success(),
            "expected success, stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let model_ids = resolved_model_ids_from_models_list_json(&output.stdout);
        let catalog_ids_seen: BTreeSet<String> = model_ids
            .intersection(&expected_catalog_ids)
            .cloned()
            .collect();
        assert!(
            catalog_ids_seen == expected_catalog_ids,
            "expected each process to resolve the same stub catalog ids; got {catalog_ids_seen:?} from {model_ids:?}"
        );
        if let Some(baseline) = &baseline_model_ids {
            assert_eq!(
                model_ids, *baseline,
                "expected concurrent runs to produce identical resolved model sets"
            );
        } else {
            baseline_model_ids = Some(model_ids);
        }
    }

    assert_eq!(
        mock.hits(),
        1,
        "expected exactly one fetch across concurrent processes"
    );
}

#[test]
#[serial]
fn scenario_j_ttl_zero_always_refreshes() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(API_PATH);
        then.status(200).json_body(sample_catalog_json());
    });

    let (temp, project_root) = setup_project(&server);

    fs::write(
        project_root.join("mars.toml"),
        "[settings]\nmodels_cache_ttl_hours = 0\n",
    )
    .expect("failed to write mars.toml with ttl=0");

    let stale_but_recent = fresh_fetched_at();
    write_cache(
        &project_root,
        sample_cached_models(),
        stale_but_recent.as_str(),
    );

    let mut cmd = mars_cmd(&project_root, temp.path(), &server.url(API_PATH));
    cmd.args(["models", "list"]);
    cmd.assert().success();

    let cache = read_cache_json(&project_root);
    let updated_fetched_at = cache["fetched_at"]
        .as_str()
        .expect("fetched_at should be present after refresh");
    assert_ne!(
        updated_fetched_at, stale_but_recent,
        "ttl=0 should force refresh even with fresh cache"
    );
    assert_eq!(mock.hits(), 1, "ttl=0 should force one network fetch");
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
    assert!(
        stdout["warning"]
            .as_str()
            .expect("passthrough warning should be present")
            .contains("passing through to harness")
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
