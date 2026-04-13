//! Shared output formatting for CLI commands.
//!
//! Supports two modes: human-readable tables and JSON.
//! Respects `NO_COLOR` env var for colored output.

use std::io::Write;

use serde::Serialize;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

use crate::diagnostic::Diagnostic;
use crate::sync::SyncReport;
use crate::sync::apply::{ActionOutcome, ActionTaken};

/// Check if colored output should be used.
///
/// Respects `NO_COLOR` env var (https://no-color.org/).
pub fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn color_choice() -> ColorChoice {
    if use_color() {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    }
}

/// Entry in the list command output.
#[derive(Debug, Serialize)]
pub struct ListEntry {
    pub source: String,
    pub item: String,
    pub kind: String,
    pub version: String,
    pub status: String,
}

/// Catalog entry — name + description for discovery.
#[derive(Debug, Serialize)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub kind: String,
}

/// Print catalog view (name: description, grouped by kind).
pub fn print_catalog(agents: &[CatalogEntry], skills: &[CatalogEntry], kind_filter: Option<&str>) {
    let show_agents =
        kind_filter.is_none() || kind_filter == Some("agents") || kind_filter == Some("agent");
    let show_skills =
        kind_filter.is_none() || kind_filter == Some("skills") || kind_filter == Some("skill");

    if show_agents && !agents.is_empty() {
        println!("AGENTS");
        for entry in agents {
            if entry.description.is_empty() {
                println!("- {}", entry.name);
            } else {
                println!("- {}: {}", entry.name, entry.description);
            }
        }
    }

    if show_agents && !agents.is_empty() && show_skills && !skills.is_empty() {
        println!();
    }

    if show_skills && !skills.is_empty() {
        println!("SKILLS");
        for entry in skills {
            if entry.description.is_empty() {
                println!("- {}", entry.name);
            } else {
                println!("- {}: {}", entry.name, entry.description);
            }
        }
    }

    if (show_agents && agents.is_empty() && show_skills && skills.is_empty())
        || (show_agents && !show_skills && agents.is_empty())
        || (show_skills && !show_agents && skills.is_empty())
    {
        println!("  no managed items");
    }
}

/// Print sync report as human-readable text or JSON.
pub fn print_sync_report(report: &SyncReport, json: bool, no_upgrade_hint: bool) {
    if json {
        print_sync_report_json(report);
    } else {
        print_sync_report_human(report, no_upgrade_hint);
    }
}

/// Whether this report is from a dry run (`--diff`).
/// Returns true when the report was produced without writing any files.
fn is_dry_run(report: &SyncReport) -> bool {
    report.dry_run
}

fn print_sync_report_json(report: &SyncReport) {
    #[derive(Serialize)]
    struct JsonTargetOutcome {
        name: String,
        synced: usize,
        removed: usize,
        errors: Vec<String>,
    }

    #[derive(Serialize)]
    struct JsonReport {
        ok: bool,
        dry_run: bool,
        installed: usize,
        updated: usize,
        removed: usize,
        conflicts: usize,
        kept: usize,
        skipped: usize,
        upgrades_available: usize,
        targets: Vec<JsonTargetOutcome>,
        diagnostics: Vec<Diagnostic>,
    }

    let mut installed = 0;
    let mut updated = 0;
    let mut removed = 0;
    let mut conflicts = 0;
    let mut kept = 0;
    let mut skipped = 0;

    for outcome in &report.applied.outcomes {
        match outcome.action {
            ActionTaken::Installed => installed += 1,
            ActionTaken::Updated => updated += 1,
            ActionTaken::Merged => updated += 1,
            ActionTaken::Conflicted => conflicts += 1,
            ActionTaken::Removed => removed += 1,
            ActionTaken::Kept => kept += 1,
            ActionTaken::Skipped => skipped += 1,
        }
    }

    for outcome in &report.pruned {
        if matches!(outcome.action, ActionTaken::Removed) {
            removed += 1;
        }
    }

    let targets = report
        .target_outcomes
        .iter()
        .map(|outcome| JsonTargetOutcome {
            name: outcome.target.clone(),
            synced: outcome.items_synced,
            removed: outcome.items_removed,
            errors: outcome.errors.clone(),
        })
        .collect();

    let json_report = JsonReport {
        ok: conflicts == 0,
        dry_run: report.dry_run,
        installed,
        updated,
        removed,
        conflicts,
        kept,
        skipped,
        upgrades_available: report.upgrades_available,
        targets,
        diagnostics: report.diagnostics.clone(),
    };

    println!(
        "{}",
        serde_json::to_string(&json_report).unwrap_or_default()
    );
}

fn print_sync_report_human(report: &SyncReport, no_upgrade_hint: bool) {
    let mut stdout = StandardStream::stdout(color_choice());

    let mut installed = 0usize;
    let mut updated = 0usize;
    let mut removed = 0usize;
    let mut conflicts = 0usize;
    let mut kept = 0usize;

    // Print per-item actions
    for outcome in &report.applied.outcomes {
        match outcome.action {
            ActionTaken::Installed => {
                installed += 1;
                print_action_line(&mut stdout, "+", Color::Green, outcome);
            }
            ActionTaken::Updated | ActionTaken::Merged => {
                updated += 1;
                print_action_line(&mut stdout, "~", Color::Yellow, outcome);
            }
            ActionTaken::Conflicted => {
                conflicts += 1;
                print_action_line(&mut stdout, "!", Color::Red, outcome);
            }
            ActionTaken::Removed => {
                removed += 1;
                print_action_line(&mut stdout, "-", Color::Red, outcome);
            }
            ActionTaken::Kept => {
                kept += 1;
            }
            ActionTaken::Skipped => {}
        }
    }

    for outcome in &report.pruned {
        if matches!(outcome.action, ActionTaken::Removed) {
            removed += 1;
            print_action_line(&mut stdout, "-", Color::Red, outcome);
        }
    }

    // Summary line — use "would ..." wording for dry runs
    let _ = writeln!(stdout);
    let dry = is_dry_run(report);
    if installed > 0 {
        if dry {
            let _ = writeln!(stdout, "  would install {installed} new items");
        } else {
            let _ = writeln!(stdout, "  installed   {installed} new items");
        }
    }
    if updated > 0 {
        if dry {
            let _ = writeln!(stdout, "  would update  {updated} items");
        } else {
            let _ = writeln!(stdout, "  updated     {updated} items");
        }
    }
    if removed > 0 {
        if dry {
            let _ = writeln!(stdout, "  would remove  {removed} orphans");
        } else {
            let _ = writeln!(stdout, "  removed     {removed} orphans");
        }
    }
    if kept > 0 {
        let _ = writeln!(stdout, "  kept        {kept} locally modified");
    }
    if conflicts > 0 {
        let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)));
        let _ = writeln!(
            stdout,
            "  conflicts   {conflicts} files (run `mars resolve` after fixing)"
        );
        let _ = stdout.reset();
    }

    if installed == 0 && updated == 0 && removed == 0 && conflicts == 0 && kept == 0 {
        let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)));
        let _ = writeln!(stdout, "  already up to date");
        let _ = stdout.reset();
    }

    // Print diagnostics to stderr so machine-readable stdout remains stable.
    let mut stderr = StandardStream::stderr(color_choice());
    for diag in &report.diagnostics {
        let color = match diag.level {
            crate::diagnostic::DiagnosticLevel::Warning => Color::Yellow,
            crate::diagnostic::DiagnosticLevel::Info => Color::Cyan,
        };
        let _ = stderr.set_color(ColorSpec::new().set_fg(Some(color)));
        let _ = writeln!(stderr, "  {diag}");
        let _ = stderr.reset();
    }

    if report.upgrades_available > 0 && !report.dry_run && !no_upgrade_hint {
        let noun = if report.upgrades_available == 1 {
            "upgrade"
        } else {
            "upgrades"
        };
        let _ = stderr.set_color(ColorSpec::new().set_fg(Some(Color::Cyan)));
        let _ = writeln!(
            stderr,
            "  ℹ {} {noun} available — run `mars upgrade --bump` to update",
            report.upgrades_available
        );
        let _ = stderr.reset();
    }
}

fn print_action_line(
    stdout: &mut StandardStream,
    prefix: &str,
    color: Color,
    outcome: &ActionOutcome,
) {
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(color)));
    let _ = write!(stdout, "  {prefix} ");
    let _ = stdout.reset();
    let _ = writeln!(
        stdout,
        "{} ({})",
        outcome.dest_path.display(),
        outcome.item_id.kind
    );
}

/// Print a list of items as a table or JSON.
pub fn print_list(entries: &[ListEntry], json: bool) {
    if json {
        println!("{}", serde_json::to_string(entries).unwrap_or_default());
    } else {
        print_list_human(entries);
    }
}

fn print_list_human(entries: &[ListEntry]) {
    if entries.is_empty() {
        println!("  no managed items");
        return;
    }

    // Compute column widths
    let source_w = entries
        .iter()
        .map(|e| e.source.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let item_w = entries
        .iter()
        .map(|e| e.item.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let version_w = entries
        .iter()
        .map(|e| e.version.len())
        .max()
        .unwrap_or(7)
        .max(7);

    // Header
    println!(
        "{:<source_w$}  {:<item_w$}  {:<version_w$}  STATUS",
        "SOURCE", "ITEM", "VERSION"
    );

    let mut stdout = StandardStream::stdout(color_choice());
    for entry in entries {
        let _ = write!(
            stdout,
            "{:<source_w$}  {:<item_w$}  {:<version_w$}  ",
            entry.source, entry.item, entry.version
        );
        let color = match entry.status.as_str() {
            "ok" => Color::Green,
            "modified" => Color::Yellow,
            "conflicted" => Color::Red,
            _ => Color::White,
        };
        let _ = stdout.set_color(ColorSpec::new().set_fg(Some(color)));
        let _ = writeln!(stdout, "{}", entry.status);
        let _ = stdout.reset();
    }
}

/// Print doctor report.
pub fn print_doctor(errors: &[String], warnings: &[String], json: bool) {
    if json {
        #[derive(Serialize)]
        struct DoctorReport {
            ok: bool,
            errors: Vec<String>,
            warnings: Vec<String>,
        }
        let report = DoctorReport {
            ok: errors.is_empty(),
            errors: errors.to_vec(),
            warnings: warnings.to_vec(),
        };
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        let mut stdout = StandardStream::stdout(color_choice());
        if errors.is_empty() && warnings.is_empty() {
            let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)));
            let _ = writeln!(stdout, "  all checks passed");
            let _ = stdout.reset();
        } else {
            for warning in warnings {
                let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)));
                let _ = write!(stdout, "  ⚠ ");
                let _ = stdout.reset();
                let _ = writeln!(stdout, "{warning}");
            }

            for error in errors {
                let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)));
                let _ = write!(stdout, "  ✗ ");
                let _ = stdout.reset();
                let _ = writeln!(stdout, "{error}");
            }
            let _ = writeln!(stdout);
            if !warnings.is_empty() {
                let _ = writeln!(stdout, "  {} warning(s)", warnings.len());
            }
            if !errors.is_empty() {
                let _ = writeln!(stdout, "  {} error(s)", errors.len());
            }
        }
    }
}

/// Print simple JSON value.
pub fn print_json<T: Serialize>(value: &T) {
    println!("{}", serde_json::to_string(value).unwrap_or_default());
}

/// Print a simple success message.
pub fn print_success(msg: &str) {
    let mut stdout = StandardStream::stdout(color_choice());
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)));
    let _ = write!(stdout, "  ✓ ");
    let _ = stdout.reset();
    let _ = writeln!(stdout, "{msg}");
}

/// Print a warning message (yellow).
pub fn print_warn(msg: &str) {
    let mut stdout = StandardStream::stdout(color_choice());
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)));
    let _ = write!(stdout, "  ⚠ ");
    let _ = stdout.reset();
    let _ = writeln!(stdout, "{msg}");
}

/// Print an error message (red).
pub fn print_error(msg: &str) {
    let mut stdout = StandardStream::stdout(color_choice());
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)));
    let _ = write!(stdout, "  ✗ ");
    let _ = stdout.reset();
    let _ = writeln!(stdout, "{msg}");
}

/// Print an info message.
pub fn print_info(msg: &str) {
    println!("  {msg}");
}
