#![allow(dead_code)]

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use httpmock::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

pub const API_PATH: &str = "/api.json";

/// Create a local path source fixture with agents and skills.
pub fn create_source(
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

pub fn create_mcp_source(dir: &TempDir, name: &str, server_name: &str) -> std::path::PathBuf {
    let source_dir = dir.child(name);
    let mcp_dir = source_dir.child("mcp").child(server_name);
    mcp_dir.create_dir_all().unwrap();
    mcp_dir
        .child("mcp.toml")
        .write_str(
            r#"
command = "npx"
args = ["-y", "example@latest"]
visibility = "exported"
"#,
        )
        .unwrap();
    source_dir.to_path_buf()
}

pub fn mars() -> Command {
    Command::cargo_bin("mars").unwrap()
}

/// Set up a minimal project with one local path source synced.
pub fn setup_synced_project(
    dir: &TempDir,
    project_name: &str,
    source_name: &str,
    agents: &[(&str, &str)],
    skills: &[(&str, &str)],
) -> std::path::PathBuf {
    let source = create_source(dir, source_name, agents, skills);
    let project = dir.child(project_name);
    project.create_dir_all().unwrap();

    let toml = format!(
        "[dependencies]\n{source_name} = {{ path = \"{}\" }}\n",
        source.display()
    );
    project.child("mars.toml").write_str(&toml).unwrap();

    mars()
        .args(["sync", "--root", project.path().to_str().unwrap()])
        .assert()
        .success();

    project.to_path_buf()
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn fresh_fetched_at() -> String {
    now_unix_secs().saturating_sub(60).to_string()
}

pub fn stale_fetched_at() -> String {
    now_unix_secs().saturating_sub(25 * 3600).to_string()
}

pub fn sample_catalog_json() -> Value {
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

pub fn sample_cached_models() -> Vec<Value> {
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

pub fn cache_path(project_root: &Path) -> PathBuf {
    project_root.join(".mars").join("models-cache.json")
}

pub fn models_merged_path(project_root: &Path) -> PathBuf {
    project_root.join(".mars").join("models-merged.json")
}

pub fn write_local_source_with_model_alias(
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

pub fn resolved_model_ids_from_models_list_json(stdout: &[u8]) -> BTreeSet<String> {
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

pub fn write_cache(project_root: &Path, models: Vec<Value>, fetched_at: &str) {
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

pub fn read_cache_json(project_root: &Path) -> Value {
    let raw = fs::read_to_string(cache_path(project_root)).expect("failed to read cache file");
    serde_json::from_str(&raw).expect("failed to parse cache file JSON")
}

pub fn read_cache_raw(project_root: &Path) -> String {
    fs::read_to_string(cache_path(project_root)).expect("failed to read cache file")
}

pub fn configure_assert_cmd(cmd: &mut Command, temp_root: &Path, api_url: &str) {
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

pub fn configure_std_cmd(cmd: &mut StdCommand, temp_root: &Path, api_url: &str) {
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

pub fn mars_cmd(project_root: &Path, temp_root: &Path, api_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("mars").expect("failed to locate mars test binary");
    configure_assert_cmd(&mut cmd, temp_root, api_url);
    cmd.arg("--root").arg(project_root);
    cmd
}

pub fn init_project(project_root: &Path, temp_root: &Path, api_url: &str) {
    fs::create_dir_all(project_root).expect("failed to create project root");

    let mut cmd = mars_cmd(project_root, temp_root, api_url);
    cmd.arg("init");
    cmd.assert().success();
}

pub fn setup_project(server: &MockServer) -> (tempfile::TempDir, PathBuf) {
    let temp = tempdir().expect("failed to create temp dir");
    let project_root = temp.path().join("project");
    init_project(&project_root, temp.path(), &server.url(API_PATH));
    (temp, project_root)
}
