//! `mars override` — set a local dev override for a source.

use std::path::Path;

use crate::config::OverrideEntry;
use crate::error::MarsError;

use super::output;

/// Arguments for `mars override`.
#[derive(Debug, clap::Args)]
pub struct OverrideArgs {
    /// Source name to override.
    pub source: String,

    /// Local path to use instead.
    #[arg(long)]
    pub path: std::path::PathBuf,
}

/// Run `mars override`.
pub fn run(args: &OverrideArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    // Validate source exists in config
    let config = crate::config::load(root)?;
    if !config.sources.contains_key(&args.source) {
        return Err(MarsError::Source {
            source_name: args.source.clone(),
            message: format!("source `{}` not found in agents.toml", args.source),
        });
    }

    // Load or create agents.local.toml
    let mut local = crate::config::load_local(root)?;
    local.overrides.insert(
        args.source.clone(),
        OverrideEntry {
            path: args.path.clone(),
        },
    );
    crate::config::save_local(root, &local)?;

    if json {
        output::print_json(&serde_json::json!({
            "ok": true,
            "source": args.source,
            "path": args.path.to_string_lossy(),
        }));
    } else {
        output::print_success(&format!(
            "override `{}` → {}",
            args.source,
            args.path.display()
        ));
        output::print_info("run `mars sync` to apply the override");
    }

    Ok(0)
}
