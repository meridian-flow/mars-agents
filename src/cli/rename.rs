//! `mars rename` — rename a managed item.

use std::path::Path;

use crate::error::MarsError;

use super::output;

/// Arguments for `mars rename`.
#[derive(Debug, clap::Args)]
pub struct RenameArgs {
    /// Current item path (e.g., agents/coder__haowjy_meridian-base.md).
    pub from: String,
    /// New item path (e.g., agents/coder.md).
    pub to: String,
}

/// Run `mars rename`.
pub fn run(args: &RenameArgs, root: &Path, json: bool) -> Result<i32, MarsError> {
    let lock = crate::lock::load(root)?;

    // Validate `from` is a managed item
    if !lock.items.contains_key(&args.from) {
        return Err(MarsError::Source {
            source_name: "rename".to_string(),
            message: format!("`{}` is not a managed item", args.from),
        });
    }

    let locked_item = &lock.items[&args.from];
    let source_name = &locked_item.source;

    // Load config and add rename entry
    let mut config = crate::config::load(root)?;
    if let Some(source_entry) = config.sources.get_mut(source_name) {
        let rename_map = source_entry.rename.get_or_insert_with(indexmap::IndexMap::new);
        rename_map.insert(args.from.clone(), args.to.clone());
    } else {
        return Err(MarsError::Source {
            source_name: source_name.clone(),
            message: format!("source `{source_name}` not found in agents.toml"),
        });
    }

    crate::config::save(root, &config)?;

    if !json {
        output::print_info(&format!("renamed {} → {}", args.from, args.to));
    }

    // Run sync
    let report = super::sync::run_sync(root, false, false, false)?;

    output::print_sync_report(&report, json);

    if report.has_conflicts() {
        Ok(1)
    } else {
        Ok(0)
    }
}
