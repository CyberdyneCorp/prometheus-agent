//! What `prometheus update --dry-run` prints (spec: distribution-and-
//! update, Update dry-run and changelog preview): the plan of
//! `pmt_update::flow::dry_run`, as text or one `--json` object.

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_update::flow::dry_run::{Entry, Plan};
use pmt_update::runtimes::lazy::human_size;

/// Prints `plan`; exits 10 when an update is available, like `--check`.
///
/// # Errors
///
/// Output errors.
pub(super) fn print(plan: &Plan, json: bool) -> Result<u8, Error> {
    if json {
        crate::output::document("update --dry-run", plan)?;
    } else {
        print!("{}", render(plan));
    }
    Ok(plan.exit_code())
}

fn render(plan: &Plan) -> String {
    if !plan.available {
        return format!(
            "no update: {} is the newest on {} ({})\nnothing was downloaded or changed\n",
            plan.from, plan.channel, plan.to
        );
    }
    let mut out = format!(
        "update available: {} → {} ({})\n",
        plan.from, plan.to, plan.channel
    );
    if let Some(size) = plan.download_size {
        let _ = writeln!(out, "download: {}", human_size(size));
    }
    let groups: [(&str, &[Entry]); 4] = [
        ("Breaking", &plan.changes.breaking),
        ("Features", &plan.changes.features),
        ("Fixes", &plan.changes.fixes),
        ("Security", &plan.changes.security),
    ];
    for (title, entries) in groups.into_iter().filter(|(_, e)| !e.is_empty()) {
        let _ = writeln!(out, "{title}:");
        for entry in entries {
            let _ = writeln!(out, "  - {} ({})", entry.text, entry.version);
        }
    }
    out.push_str(&migrations(plan));
    if plan.runtimes.is_empty() {
        out.push_str("runtime pins: unchanged\n");
    } else {
        out.push_str("runtime pins:\n");
        for change in &plan.runtimes {
            let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "none".to_owned());
            let _ = writeln!(
                out,
                "  {}: {} → {}",
                change.runtime,
                show(&change.from),
                show(&change.to)
            );
        }
    }
    let _ = writeln!(
        out,
        "daemon restart: {}",
        if plan.daemon_restart {
            "yes (the running daemon restarts on the new build)"
        } else {
            "no daemon running"
        }
    );
    out.push_str("nothing was downloaded or changed\n");
    out
}

fn migrations(plan: &Plan) -> String {
    if plan.migrations.is_empty() {
        return "migrations: none\n".to_owned();
    }
    let mut out = String::from("migrations:\n");
    for migration in &plan.migrations {
        let mut steps = Vec::new();
        if let Some(step) = migration.config {
            steps.push(format!("config v{}→v{}", step.from, step.to));
        }
        if let Some(step) = migration.store {
            steps.push(format!("state.db v{}→v{}", step.from, step.to));
        }
        let _ = writeln!(out, "  {}: {}", migration.profile, steps.join(", "));
    }
    out
}
