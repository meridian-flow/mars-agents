use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use super::opencode::OpenCodeProbeResult;
use crate::error::MarsError;

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_TTL_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeCacheEntry {
    pub schema_version: u32,
    pub fetched_at: u64,
    pub last_attempt_at: u64,
    pub last_error: Option<String>,
    pub result: Option<OpenCodeProbeResult>,
}

#[derive(Debug, Clone)]
pub enum CachedProbeOutcome {
    Hit(OpenCodeProbeResult),
    Stale(OpenCodeProbeResult),
    Miss(OpenCodeProbeResult),
    Unavailable,
}

impl CachedProbeOutcome {
    pub fn result(&self) -> Option<&OpenCodeProbeResult> {
        match self {
            Self::Hit(r) | Self::Stale(r) | Self::Miss(r) => Some(r),
            Self::Unavailable => None,
        }
    }

    pub fn cache_status(&self) -> &'static str {
        match self {
            Self::Hit(_) => "hit",
            Self::Stale(_) => "stale",
            Self::Miss(_) => "miss",
            Self::Unavailable => "skipped",
        }
    }
}

fn cache_dir() -> Result<PathBuf, MarsError> {
    let root = crate::platform::cache::global_cache_root()?;
    Ok(root.join("availability"))
}

fn cache_path() -> Result<PathBuf, MarsError> {
    Ok(cache_dir()?.join("opencode-probe.json"))
}

fn lock_path() -> Result<PathBuf, MarsError> {
    Ok(cache_dir()?.join(".opencode-probe.lock"))
}

fn ttl_secs() -> u64 {
    std::env::var("MARS_PROBE_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_fresh(entry: &ProbeCacheEntry) -> bool {
    let ttl = ttl_secs();
    let now = now_unix_secs();
    if entry.fetched_at > now {
        return false;
    }
    (now - entry.fetched_at) < ttl
}

fn is_usable(entry: &ProbeCacheEntry) -> bool {
    entry
        .result
        .as_ref()
        .is_some_and(|r| r.provider_probe_success)
}

fn read_cache_tolerant() -> Option<ProbeCacheEntry> {
    read_cache_tolerant_at(&cache_path().ok()?)
}

fn read_cache_tolerant_at(path: &Path) -> Option<ProbeCacheEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let entry: ProbeCacheEntry = serde_json::from_str(&content).ok()?;
    if entry.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(entry)
}

fn write_cache(entry: &ProbeCacheEntry) -> Result<(), MarsError> {
    write_cache_at(&cache_path()?, entry)
}

fn write_cache_at(path: &Path, entry: &ProbeCacheEntry) -> Result<(), MarsError> {
    let json = serde_json::to_string_pretty(entry)
        .map_err(|e| MarsError::Internal(format!("probe cache serialize: {e}")))?;
    crate::fs::atomic_write(path, json.as_bytes())
}

struct FileLock {
    _file: std::fs::File,
}

fn try_lock() -> Option<FileLock> {
    lock_at(&lock_path().ok()?, true)
}

fn blocking_lock() -> Option<FileLock> {
    lock_at(&lock_path().ok()?, false)
}

fn lock_at(path: &Path, nonblocking: bool) -> Option<FileLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let flags = if nonblocking {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_EX
        };
        let ret = unsafe { libc::flock(file.as_raw_fd(), flags) };
        if ret != 0 {
            return None;
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        };
        let handle = file.as_raw_handle() as HANDLE;
        let mut overlapped = unsafe { std::mem::zeroed() };
        let flags = if nonblocking {
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
        } else {
            LOCKFILE_EXCLUSIVE_LOCK
        };
        let ret = unsafe { LockFileEx(handle, flags, 0, 1, 0, &mut overlapped) };
        if ret == 0 {
            return None;
        }
    }

    Some(FileLock { _file: file })
}

pub fn probe_cached(installed: &HashSet<String>, is_offline: bool) -> CachedProbeOutcome {
    if !super::should_probe_opencode(installed, is_offline) {
        return CachedProbeOutcome::Unavailable;
    }

    probe_cached_impl(
        is_offline,
        &cache_path().ok(),
        super::opencode::probe,
        || spawn_detached_refresh().map_err(|_| ()),
    )
}

fn probe_cached_impl<F, S>(
    is_offline: bool,
    path: &Option<PathBuf>,
    probe: F,
    spawn_refresh: S,
) -> CachedProbeOutcome
where
    F: Fn() -> OpenCodeProbeResult,
    S: Fn() -> Result<(), ()>,
{
    let cached = path.as_deref().and_then(read_cache_tolerant_at);

    match cached {
        Some(entry) if is_fresh(&entry) && is_usable(&entry) => {
            CachedProbeOutcome::Hit(entry.result.unwrap())
        }
        Some(entry) if is_usable(&entry) => {
            let result = entry.result.clone().unwrap();
            if !is_offline {
                trigger_background_refresh_with(spawn_refresh);
            }
            CachedProbeOutcome::Stale(result)
        }
        _ if is_offline => CachedProbeOutcome::Unavailable,
        _ => synchronous_probe_with(path, probe),
    }
}

fn trigger_background_refresh_with<S>(spawn_refresh: S)
where
    S: Fn() -> Result<(), ()>,
{
    let Some(lock) = try_lock() else { return };
    if let Some(entry) = read_cache_tolerant()
        && is_fresh(&entry)
        && is_usable(&entry)
    {
        drop(lock);
        return;
    }
    let _ = spawn_refresh();
    drop(lock);
}

fn synchronous_probe_with<F>(path: &Option<PathBuf>, probe: F) -> CachedProbeOutcome
where
    F: Fn() -> OpenCodeProbeResult,
{
    let lock = blocking_lock();

    if lock.is_some()
        && let Some(path) = path
        && let Some(entry) = read_cache_tolerant_at(path)
        && is_usable(&entry)
    {
        if is_fresh(&entry) {
            return CachedProbeOutcome::Hit(entry.result.unwrap());
        }
        let probe_result = probe();
        write_probe_attempt(path, probe_result.clone());
        return if probe_result.provider_probe_success {
            CachedProbeOutcome::Miss(probe_result)
        } else {
            CachedProbeOutcome::Stale(entry.result.unwrap())
        };
    }

    let probe_result = probe();
    if let Some(path) = path {
        write_probe_attempt(path, probe_result.clone());
    }
    drop(lock);

    if probe_result.provider_probe_success {
        CachedProbeOutcome::Miss(probe_result)
    } else {
        CachedProbeOutcome::Unavailable
    }
}

fn write_probe_attempt(path: &Path, probe_result: OpenCodeProbeResult) {
    let now = now_unix_secs();
    let entry = ProbeCacheEntry {
        schema_version: SCHEMA_VERSION,
        fetched_at: now,
        last_attempt_at: now,
        last_error: if probe_result.provider_probe_success {
            None
        } else {
            probe_result.error.clone()
        },
        result: Some(probe_result),
    };

    if let Err(e) = write_cache_at(path, &entry) {
        eprintln!("debug: probe cache write failed: {e}");
    }
}

fn spawn_detached_refresh() -> std::io::Result<()> {
    let mars_bin = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(mars_bin);
    cmd.args(["models", "__refresh-probe", "--target", "opencode"]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008);
    }

    cmd.spawn()?;
    Ok(())
}

pub fn run_refresh_probe_command() -> Result<i32, MarsError> {
    let Some(_lock) = blocking_lock() else {
        return Ok(0);
    };

    if let Some(entry) = read_cache_tolerant()
        && is_fresh(&entry)
        && is_usable(&entry)
    {
        return Ok(0);
    }

    let probe_result = super::opencode::probe();
    let now = now_unix_secs();
    let existing = read_cache_tolerant();

    let entry = if probe_result.provider_probe_success {
        ProbeCacheEntry {
            schema_version: SCHEMA_VERSION,
            fetched_at: now,
            last_attempt_at: now,
            last_error: None,
            result: Some(probe_result),
        }
    } else {
        ProbeCacheEntry {
            schema_version: SCHEMA_VERSION,
            fetched_at: existing.as_ref().map(|e| e.fetched_at).unwrap_or(0),
            last_attempt_at: now,
            last_error: probe_result.error.clone(),
            result: existing.and_then(|e| e.result),
        }
    };
    let _ = write_cache(&entry);

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tempfile::TempDir;

    fn ok_result() -> OpenCodeProbeResult {
        OpenCodeProbeResult {
            providers: std::collections::HashMap::from([("openai".to_string(), true)]),
            model_slugs: vec!["openai/gpt-5.4".to_string()],
            provider_probe_success: true,
            model_probe_success: true,
            error: None,
        }
    }

    fn fail_result() -> OpenCodeProbeResult {
        OpenCodeProbeResult {
            provider_probe_success: false,
            error: Some("boom".to_string()),
            ..OpenCodeProbeResult::default()
        }
    }

    fn entry(fetched_at: u64, result: Option<OpenCodeProbeResult>) -> ProbeCacheEntry {
        ProbeCacheEntry {
            schema_version: SCHEMA_VERSION,
            fetched_at,
            last_attempt_at: fetched_at,
            last_error: None,
            result,
        }
    }

    fn cache_file(temp: &TempDir) -> PathBuf {
        temp.path().join("availability").join("opencode-probe.json")
    }

    fn write_entry(path: &Path, entry: &ProbeCacheEntry) {
        write_cache_at(path, entry).unwrap();
    }

    #[test]
    fn fresh_hit_returns_cached_result() {
        let temp = TempDir::new().unwrap();
        let path = cache_file(&temp);
        write_entry(&path, &entry(now_unix_secs(), Some(ok_result())));

        let outcome = probe_cached_impl(false, &Some(path), fail_result, || Ok(()));
        assert!(matches!(outcome, CachedProbeOutcome::Hit(_)));
        assert_eq!(outcome.result().unwrap().model_slugs[0], "openai/gpt-5.4");
    }

    #[test]
    fn stale_entry_returns_stale_outcome() {
        let temp = TempDir::new().unwrap();
        let path = cache_file(&temp);
        write_entry(&path, &entry(1, Some(ok_result())));

        let outcome = probe_cached_impl(false, &Some(path), fail_result, || Ok(()));
        assert!(matches!(outcome, CachedProbeOutcome::Stale(_)));
    }

    #[test]
    fn missing_cache_runs_synchronous_probe() {
        let temp = TempDir::new().unwrap();
        let path = cache_file(&temp);
        let called = Cell::new(false);
        let outcome = probe_cached_impl(
            false,
            &Some(path.clone()),
            || {
                called.set(true);
                ok_result()
            },
            || Ok(()),
        );

        assert!(called.get());
        assert!(matches!(outcome, CachedProbeOutcome::Miss(_)));
        assert!(read_cache_tolerant_at(&path).is_some());
    }

    #[test]
    fn invalid_json_is_cache_miss() {
        let temp = TempDir::new().unwrap();
        let path = cache_file(&temp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();

        let outcome = probe_cached_impl(false, &Some(path), ok_result, || Ok(()));
        assert!(matches!(outcome, CachedProbeOutcome::Miss(_)));
    }

    #[test]
    fn incompatible_schema_is_cache_miss() {
        let temp = TempDir::new().unwrap();
        let path = cache_file(&temp);
        let mut old = entry(now_unix_secs(), Some(ok_result()));
        old.schema_version = 999;
        write_entry(&path, &old);

        let outcome = probe_cached_impl(false, &Some(path), ok_result, || Ok(()));
        assert!(matches!(outcome, CachedProbeOutcome::Miss(_)));
    }

    #[test]
    fn future_fetched_at_is_stale() {
        let future = entry(now_unix_secs() + 3600, Some(ok_result()));
        assert!(!is_fresh(&future));
    }

    #[test]
    fn ttl_override_controls_freshness() {
        let _guard = EnvGuard::set("MARS_PROBE_CACHE_TTL_SECS", "9999");
        let recent = entry(now_unix_secs().saturating_sub(10), Some(ok_result()));
        assert!(is_fresh(&recent));
    }

    #[test]
    fn write_failure_degrades_gracefully() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("availability");
        std::fs::write(&path, "file blocks directory").unwrap();
        let blocked = path.join("opencode-probe.json");

        let outcome = probe_cached_impl(false, &Some(blocked), ok_result, || Ok(()));
        assert!(matches!(outcome, CachedProbeOutcome::Miss(_)));
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                unsafe { std::env::set_var(self.key, prev) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }
}
