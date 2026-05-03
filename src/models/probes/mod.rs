use std::collections::HashSet;

pub mod opencode;
pub mod opencode_cache;

pub use opencode::{OpenCodeProbeResult, probe, probe_with_timeout};

/// Determine whether an OpenCode probe should be attempted.
/// Returns false if offline or opencode is not installed.
pub fn should_probe_opencode(installed: &HashSet<String>, is_offline: bool) -> bool {
    !is_offline && installed.contains("opencode")
}
