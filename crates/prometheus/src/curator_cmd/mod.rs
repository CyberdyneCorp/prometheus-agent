//! `prometheus curator` (spec: skills, Curator CLI; Curator configuration
//! and scheduling; Curator transitions, scope and backups; Curator audit
//! ledger, single-edit rollback, purge and reports) and `/curator`.

pub(crate) mod auto;
pub(crate) mod ops;
mod review;
pub(crate) mod slash;

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_core::error::Error;
use pmt_skills::curator::report::RunReport;
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};
pub(crate) use ops::{Curator, ledger_text};

/// `prometheus curator` subcommands.
#[derive(Debug, Subcommand)]
pub enum CuratorCommand {
    /// Show whether automatic passes run, the managed skills and what a pass would do now.
    Status,
    /// Run a pass now (in the background unless --sync).
    Run {
        /// Wait for the pass and print its summary.
        #[arg(long)]
        sync: bool,
        /// Print the planned transitions and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Also merge overlapping skills (a model review on `auxiliary.curator`).
        #[arg(long)]
        consolidate: bool,
    },
    /// Stop automatic passes.
    Pause,
    /// Allow automatic passes again.
    Resume,
    /// Keep a skill out of every curator transition.
    Pin {
        /// The skill.
        name: String,
    },
    /// Let the curator manage a pinned skill again.
    Unpin {
        /// The skill.
        name: String,
    },
    /// Put one of the profile's own skills under curator management.
    Adopt {
        /// The skill.
        name: String,
    },
    /// List the profile's own skills the curator does not manage.
    ListUnmanaged,
    /// List archived skills.
    ListArchived,
    /// Move a skill into `skills/.archive/`.
    Archive {
        /// The skill.
        name: String,
    },
    /// Move an archived skill back.
    Restore {
        /// The skill.
        name: String,
    },
    /// Archive managed skills idle for at least N days.
    Prune {
        /// Idle days.
        #[arg(long, value_name = "N")]
        days: u64,
        /// List them and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete archived skills older than `curator.archive_ttl_secs` (or --days); never automatic.
    Purge {
        /// Age in days instead of `curator.archive_ttl_secs`.
        #[arg(long, value_name = "N")]
        days: Option<u64>,
        /// List them and delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a backup of the skills tree to `skills/.backups/`.
    Backup,
    /// Undo one ledger entry (`L<n>`), or restore the newest backup without one.
    Rollback {
        /// The ledger entry.
        entry: Option<String>,
    },
    /// List skill mutations, newest first.
    Ledger {
        /// Only this skill.
        #[arg(long, value_name = "S")]
        skill: Option<String>,
        /// At most this many entries.
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
    },
}

/// Runs one `curator` subcommand.
///
/// # Errors
///
/// The command's failure.
pub fn run(command: &CuratorCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let curator = Curator::open(ctx.scope()?, ctx.launch.clone())?;
    let say = |text: String| -> Result<u8, Error> {
        if ctx.json {
            return json(&json!({"message": text}));
        }
        print_raw(&format!("{text}\n"))
    };
    match command {
        CuratorCommand::Status => status(&curator, ctx.json),
        CuratorCommand::Run {
            sync,
            dry_run,
            consolidate,
        } => run_pass(&curator, ctx, *sync || *dry_run, *dry_run, *consolidate),
        CuratorCommand::Pause => say(curator.pause(true)?),
        CuratorCommand::Resume => say(curator.pause(false)?),
        CuratorCommand::Pin { name } => say(curator.pin(name, true)?),
        CuratorCommand::Unpin { name } => say(curator.pin(name, false)?),
        CuratorCommand::Adopt { name } => say(curator.adopt(name)?),
        CuratorCommand::ListUnmanaged => names(ctx.json, "unmanaged", &curator.unmanaged()?),
        CuratorCommand::ListArchived => archived(ctx.json, &curator.archived()),
        CuratorCommand::Archive { name } => say(curator.archive(name)?),
        CuratorCommand::Restore { name } => say(curator.restore(name)?),
        CuratorCommand::Prune { days, dry_run } => prune(&curator, ctx.json, *days, *dry_run),
        CuratorCommand::Purge { days, dry_run } => purge(&curator, ctx.json, *days, *dry_run),
        CuratorCommand::Backup => say(format!("Wrote {}.", curator.backup()?.display())),
        CuratorCommand::Rollback { entry } => say(curator.rollback(entry.as_deref())?),
        CuratorCommand::Ledger { skill, limit } => {
            let entries = curator.entries(skill.as_deref(), *limit)?;
            if ctx.json {
                return json(&json!({"entries": entries}));
            }
            print_raw(&ledger_text(&*curator.text, &entries))
        }
    }
}

fn status(curator: &Curator, as_json: bool) -> Result<u8, Error> {
    let status = curator.status()?;
    if as_json {
        return json(&status);
    }
    let mut text = String::new();
    let state = match (status.enabled, status.paused) {
        (false, _) => "disabled (curator.enabled = false)",
        (true, true) => "paused",
        (true, false) => "enabled",
    };
    let _ = writeln!(text, "curator: {state}");
    let _ = writeln!(
        text,
        "last pass: {}",
        status.last_run_at.as_deref().unwrap_or("never")
    );
    let _ = writeln!(text, "managed: {}", list(&status.managed));
    let _ = writeln!(text, "pinned: {}", list(&status.pinned));
    let _ = writeln!(text, "archived: {}", status.archived);
    let _ = writeln!(
        text,
        "a pass now would make {} transition(s)",
        status.planned.len()
    );
    print_raw(&text)
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// The text of a pass (the transitions, then the summary).
pub(crate) fn report_text(catalog: &dyn pmt_core::text::Catalog, report: &RunReport) -> String {
    let mut text = String::new();
    for t in &report.transitions {
        let idle = catalog.text("slash.curator.idle", &[("days", &t.idle_days)]);
        let _ = writeln!(text, "{}: {} -> {} ({idle})", t.name, t.from, t.to);
    }
    for e in &report.errors {
        let line = catalog.text("slash.curator.problem", &[("problem", e)]);
        let _ = writeln!(text, "{line}");
    }
    let _ = writeln!(text, "{}", report.summary_in(catalog));
    text
}

fn run_pass(
    curator: &Curator,
    ctx: &Context<'_>,
    sync: bool,
    dry_run: bool,
    consolidate: bool,
) -> Result<u8, Error> {
    if !sync {
        return background(ctx, consolidate);
    }
    let report = curator.run(dry_run, consolidate, false)?;
    if ctx.json {
        return json(&report);
    }
    print_raw(&report_text(&*curator.text, &report))
}

/// Starts `curator run --sync` as a detached process.
fn background(ctx: &Context<'_>, consolidate: bool) -> Result<u8, Error> {
    let exe = std::env::current_exe().map_err(|e| {
        Error::new(
            &pmt_core::error::codes::OUTPUT_IO,
            format!("cannot find the executable: {e}"),
        )
    })?;
    let mut command = std::process::Command::new(exe);
    if let Some(home) = ctx.home_flag {
        command.arg("--home").arg(home);
    }
    if let Some(profile) = ctx.profile {
        command.args(["--profile", profile]);
    }
    command.args(["curator", "run", "--sync"]);
    if consolidate {
        command.arg("--consolidate");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command.spawn().map_err(|e| {
        Error::new(
            &pmt_core::error::codes::OUTPUT_IO,
            format!("cannot start the pass: {e}"),
        )
    })?;
    let text = format!(
        "curator pass started in the background (pid {}); its report lands under logs/curator/",
        child.id()
    );
    if ctx.json {
        return json(&json!({"pid": child.id(), "message": text}));
    }
    print_raw(&format!("{text}\n"))
}

fn names(as_json: bool, key: &str, names: &[String]) -> Result<u8, Error> {
    if as_json {
        return json(&json!({ key: names }));
    }
    print_raw(&format!(
        "{}\n",
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join("\n")
        }
    ))
}

fn archived(as_json: bool, archived: &[pmt_skills::curator::moves::Archived]) -> Result<u8, Error> {
    if as_json {
        return json(&json!({"archived": archived}));
    }
    let mut text = String::new();
    for a in archived {
        let _ = writeln!(
            text,
            "{}  skills/{}  {}",
            a.name,
            a.rel,
            a.archived_at.as_deref().unwrap_or("")
        );
    }
    if text.is_empty() {
        text.push_str("no archived skills\n");
    }
    print_raw(&text)
}

fn prune(curator: &Curator, as_json: bool, days: u64, dry_run: bool) -> Result<u8, Error> {
    if days == 0 {
        return Err(Error::new(
            &pmt_core::error::codes::CLI_USAGE,
            "--days must be at least 1",
        ));
    }
    let rows = curator.prune(days, dry_run)?;
    if as_json {
        return json(
            &json!({"dry_run": dry_run, "skills": rows.iter().map(|(n, d)| json!({"name": n, "idle_days": d})).collect::<Vec<_>>()}),
        );
    }
    let mut text = String::new();
    let verb = if dry_run { "would archive" } else { "archived" };
    for (name, idle) in &rows {
        let _ = writeln!(text, "{verb} {name} (idle {idle} days)");
    }
    if rows.is_empty() {
        let _ = writeln!(text, "nothing idle for {days} days or more");
    }
    print_raw(&text)
}

fn purge(curator: &Curator, as_json: bool, days: Option<u64>, dry_run: bool) -> Result<u8, Error> {
    let rows = curator.purge(days, dry_run)?;
    if as_json {
        return json(&json!({"dry_run": dry_run, "purged": rows}));
    }
    let mut text = String::new();
    let verb = if dry_run { "would delete" } else { "deleted" };
    for a in &rows {
        let _ = writeln!(text, "{verb} skills/{}", a.rel);
    }
    if rows.is_empty() {
        text.push_str("no archived skill is old enough\n");
    }
    print_raw(&text)
}
