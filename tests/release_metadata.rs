use std::path::Path;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).expect(path)
}

#[test]
fn npm_stub_declares_windows_optional_package() {
    let stub: serde_json::Value =
        serde_json::from_str(&read("npm/@meridian-flow/mars-agents/package.json"))
            .expect("stub package json");
    let optional = stub
        .get("optionalDependencies")
        .and_then(serde_json::Value::as_object)
        .expect("optionalDependencies");

    assert!(
        optional.contains_key("@meridian-flow/mars-agents-win32-x64"),
        "stub package must install the Windows x64 binary package"
    );
}

#[test]
fn windows_npm_package_publishes_exe_only_for_win32_x64() {
    let pkg: serde_json::Value = serde_json::from_str(&read(
        "npm/@meridian-flow/mars-agents-win32-x64/package.json",
    ))
    .expect("windows package json");

    assert_eq!(
        pkg.get("name").and_then(serde_json::Value::as_str),
        Some("@meridian-flow/mars-agents-win32-x64")
    );
    assert_eq!(
        pkg.get("os").and_then(serde_json::Value::as_array),
        Some(&vec![serde_json::Value::String("win32".to_string())])
    );
    assert_eq!(
        pkg.get("cpu").and_then(serde_json::Value::as_array),
        Some(&vec![serde_json::Value::String("x64".to_string())])
    );
    assert_eq!(
        pkg.get("files").and_then(serde_json::Value::as_array),
        Some(&vec![serde_json::Value::String("mars.exe".to_string())])
    );
}

#[test]
fn npm_launcher_routes_windows_to_exe_package() {
    let launcher = read("npm/@meridian-flow/mars-agents/bin/mars");

    assert!(launcher.contains("\"win32 x64\": \"@meridian-flow/mars-agents-win32-x64\""));
    assert!(launcher.contains("process.platform === \"win32\" ? \"mars.exe\" : \"mars\""));
    assert!(launcher.contains("win32-x64"));
}

#[test]
fn release_workflow_builds_and_publishes_windows_artifacts() {
    let workflow = read(".github/workflows/release.yml");

    assert!(workflow.contains("x86_64-pc-windows-msvc"));
    assert!(workflow.contains("artifact: mars-windows-x64.exe"));
    assert!(workflow.contains("Smoke test (Windows)"));
    assert!(workflow.contains("mars.exe --version"));
    assert!(workflow.contains("mars.exe init --root $tmp"));
    assert!(workflow.contains("mars.exe doctor --root $tmp"));
    assert!(workflow.contains("cp \"$GITHUB_WORKSPACE/artifacts/$binary\" mars.exe"));
    assert!(workflow.contains(
        "publish_platform npm/@meridian-flow/mars-agents-win32-x64 mars-windows-x64.exe"
    ));
}

#[test]
fn ci_workflow_runs_windows_build_test_clippy_and_fmt() {
    let workflow = read(".github/workflows/ci.yml");

    assert!(workflow.contains("check-windows:"));
    assert!(workflow.contains("runs-on: windows-latest"));
    assert!(workflow.contains("cargo build"));
    assert!(workflow.contains("cargo test"));
    assert!(workflow.contains("cargo clippy -- -D warnings"));
    assert!(workflow.contains("cargo fmt --check"));
}
