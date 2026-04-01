//! Shared output formatting for CLI commands.
//!
//! Supports two modes: human-readable tables and JSON.
//! Respects `NO_COLOR` env var for colored output.

use std::io::Write;

use serde::Serialize;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

use crate::sync::apply::{ActionOutcome, ActionTaken};
use crate::sync::SyncReport;
use crate::validate::ValidationWarning;

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

/// Print sync report as human-readable text or JSON.
pub fn print_sync_report(report: &SyncReport, json: bool) {
    if json {
        print_sync_report_json(report);
    } else {
        print_sync_report_human(report);
    }
}

fn print_sync_report_json(report: &SyncReport) {
    #[derive(Serialize)]
    struct JsonReport {
        ok: bool,
        installed: usize,
        updated: usize,
        removed: usize,
        conflicts: usize,
        kept: usize,
        skipped: usize,
        warnings: Vec<String>,
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

    let warnings: Vec<String> = report.warnings.iter().map(format_warning).collect();

    let json_report = JsonReport {
        ok: conflicts == 0,
        installed,
        updated,
        removed,
        conflicts,
        kept,
        skipped,
        warnings,
    };

    println!(
        "{}",
        serde_json::to_string(&json_report).unwrap_or_default()
    );
}

fn print_sync_report_human(report: &SyncReport) {
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

    // Summary line
    let _ = writeln!(stdout);
    if installed > 0 {
        let _ = writeln!(stdout, "  installed   {installed} new items");
    }
    if updated > 0 {
        let _ = writeln!(stdout, "  updated     {updated} items");
    }
    if removed > 0 {
        let _ = writeln!(stdout, "  removed     {removed} orphans");
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

    // Print warnings
    for warning in &report.warnings {
        let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)));
        let _ = writeln!(stdout, "  warning: {}", format_warning(warning));
        let _ = stdout.reset();
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

fn format_warning(w: &ValidationWarning) -> String {
    match w {
        ValidationWarning::MissingSkill {
            agent,
            skill_name,
            suggestion,
        } => {
            let base = format!(
                "agent `{}` references missing skill `{}`",
                agent.name, skill_name
            );
            match suggestion {
                Some(s) => format!("{base} (did you mean `{s}`?)"),
                None => base,
            }
        }
        ValidationWarning::OrphanedSkill { skill } => {
            format!("skill `{}` is installed but not referenced by any agent", skill.name)
        }
    }
}

/// Print a list of items as a table or JSON.
pub fn print_list(entries: &[ListEntry], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(entries).unwrap_or_default()
        );
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
pub fn print_doctor(issues: &[String], json: bool) {
    if json {
        #[derive(Serialize)]
        struct DoctorReport {
            ok: bool,
            issues: Vec<String>,
        }
        let report = DoctorReport {
            ok: issues.is_empty(),
            issues: issues.to_vec(),
        };
        println!(
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        );
    } else {
        let mut stdout = StandardStream::stdout(color_choice());
        if issues.is_empty() {
            let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)));
            let _ = writeln!(stdout, "  all checks passed");
            let _ = stdout.reset();
        } else {
            for issue in issues {
                let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Red)));
                let _ = write!(stdout, "  ✗ ");
                let _ = stdout.reset();
                let _ = writeln!(stdout, "{issue}");
            }
            let _ = writeln!(stdout);
            let _ = writeln!(
                stdout,
                "  {} issues found. Run `mars repair` to fix.",
                issues.len()
            );
        }
    }
}

/// Print simple JSON value.
pub fn print_json<T: Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string(value).unwrap_or_default()
    );
}

/// Print a simple success message.
pub fn print_success(msg: &str) {
    let mut stdout = StandardStream::stdout(color_choice());
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(Color::Green)));
    let _ = write!(stdout, "  ✓ ");
    let _ = stdout.reset();
    let _ = writeln!(stdout, "{msg}");
}

/// Print an info message.
pub fn print_info(msg: &str) {
    println!("  {msg}");
}
