mod common;

use assert_cmd::cargo::cargo_bin;
use httpmock::prelude::*;
use serde_json::Value;
use serial_test::serial;
use std::collections::BTreeSet;
use std::fs;
use std::process::{Command as StdCommand, Output};
use std::thread;
use std::time::Duration;

use common::*;

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
                    .arg("--unavailable")
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
