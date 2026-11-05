//! `prometheus files`: undo a session's file changes, roll a workspace
//! back to a checkpoint, and manage the snapshot store (spec:
//! execution-environments, File snapshots and undo; Rollback listing,
//! diff and selective restore; Snapshot store management CLI).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_tools::snapshots::SnapshotStore;
use pmt_tools::snapshots::manage::{self, PrunePolicy};
use pmt_tools::snapshots::restore;

use crate::context::Context;

/// `prometheus files` subcommands.
#[derive(Debug, Subcommand)]
pub enum FilesCommand {
    /// Undo the file changes of a session's last turns.
    Undo(UndoArgs),
    /// List a workspace's checkpoints, show one's diff, or restore it.
    Rollback(RollbackArgs),
    /// The snapshot store: status, prune, clear.
    Snapshots {
        /// The snapshots subcommand.
        #[command(subcommand)]
        command: SnapshotsCommand,
    },
}

/// `prometheus files undo`.
#[derive(Debug, Args)]
pub struct UndoArgs {
    /// The session whose changes are undone.
    #[arg(long)]
    pub session: String,
    /// How many of its last turns.
    #[arg(long, default_value_t = 1)]
    pub turn: u64,
    /// Show what would be restored without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Overwrite files changed since the session wrote them.
    #[arg(long)]
    pub force: bool,
}

/// `prometheus files rollback [N] [FILE]`.
#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// The checkpoint (1 is the newest); none lists them.
    pub checkpoint: Option<usize>,
    /// Restore only this file (relative to the project root).
    pub file: Option<String>,
    /// Show the diff between the checkpoint and now; change nothing.
    #[arg(long)]
    pub diff: bool,
    /// Overwrite files edited by hand after the agent's last write too.
    #[arg(long)]
    pub all: bool,
    /// The workspace (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

/// `prometheus files snapshots` subcommands.
#[derive(Debug, Subcommand)]
pub enum SnapshotsCommand {
    /// Total size, workspaces, and per workspace size, checkpoints, last use.
    #[command(alias = "list")]
    Status {
        /// Workspaces listed.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Delete unused workspaces and enforce the size cap.
    Prune {
        /// Delete workspaces unused for longer than this.
        #[arg(long, default_value_t = 7)]
        retention_days: u64,
        /// Size cap in MB (default `files.snapshot.max_bytes`).
        #[arg(long)]
        max_mb: Option<u64>,
        /// Keep workspaces whose directory is missing.
        #[arg(long)]
        keep_orphans: bool,
    },
    /// Delete the whole store.
    Clear {
        /// Do not ask.
        #[arg(short = 'f', long)]
        force: bool,
    },
}

/// Runs `prometheus files`.
///
/// # Errors
///
/// No profile, an unknown session or checkpoint, store I/O.
pub fn run(command: &FilesCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = SnapshotStore::of_profile(scope.dir());
    match command {
        FilesCommand::Undo(args) => undo(&scope, &store, args, ctx.json),
        FilesCommand::Rollback(args) => rollback(&store, args, ctx.json),
        FilesCommand::Snapshots { command } => snapshots(&scope, &store, command, ctx.json),
    }
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HOME_IO, path, err)
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

fn undo(
    scope: &pmt_config::ProfileScope,
    store: &SnapshotStore,
    args: &UndoArgs,
    json: bool,
) -> Result<u8, Error> {
    let sessions = pmt_store::Store::open(scope)?;
    let id = sessions.resolve_session(&args.session)?;
    let turn = sessions.user_turns(id)?;
    let report = restore::undo(
        store,
        &id.to_string(),
        Some(turn),
        args.turn,
        args.force,
        args.dry_run,
    )
    .map_err(|e| io(store.root(), &e))?;
    if json {
        print_json(&report);
    } else {
        print!("{}", undo_text(&report, args.dry_run));
    }
    if !args.dry_run && !report.turns.is_empty() && report.conflicts.is_empty() {
        crate::chat::commands::files::note_undo(&sessions, id, &report)?;
    }
    Ok(if report.conflicts.is_empty() {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::FAILURE
    })
}

/// The undo report for a person.
#[must_use]
pub fn undo_text(report: &restore::UndoReport, dry_run: bool) -> String {
    let mut text = String::new();
    let (restored, removed) = if dry_run {
        ("would restore", "would remove")
    } else {
        ("restored", "removed")
    };
    for path in &report.restored {
        let _ = writeln!(text, "{restored}: {}", path.display());
    }
    for path in &report.removed {
        let _ = writeln!(text, "{removed}: {}", path.display());
    }
    for conflict in &report.conflicts {
        let _ = writeln!(text, "{conflict}");
    }
    if text.is_empty() {
        text.push_str("no file changes to undo\n");
    }
    text
}

fn rollback(store: &SnapshotStore, args: &RollbackArgs, json: bool) -> Result<u8, Error> {
    let dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|e| io(Path::new("."), &e))?,
    };
    let reply = crate::chat::commands::files::rollback_reply(
        store,
        &dir,
        args.checkpoint,
        args.file.as_deref(),
        args.diff,
        args.all,
    )?;
    if json {
        print_json(&serde_json::json!({ "output": reply }));
    } else {
        println!("{}", reply.trim_end());
    }
    Ok(pmt_core::exit::SUCCESS)
}

fn snapshots(
    scope: &pmt_config::ProfileScope,
    store: &SnapshotStore,
    command: &SnapshotsCommand,
    json: bool,
) -> Result<u8, Error> {
    match command {
        SnapshotsCommand::Status { limit } => {
            let status = manage::status(store, *limit);
            if json {
                print_json(&status);
            } else {
                print!("{}", status_text(&status));
            }
            Ok(pmt_core::exit::SUCCESS)
        }
        SnapshotsCommand::Prune {
            retention_days,
            max_mb,
            keep_orphans,
        } => {
            let max_bytes = max_mb.map_or(scope.config().files.snapshot.max_bytes, |mb| {
                mb.saturating_mul(1024 * 1024)
            });
            let policy = PrunePolicy {
                retention: Duration::from_hours(24 * retention_days),
                max_bytes,
                orphans: !keep_orphans,
            };
            let pruned = manage::prune(store, policy).map_err(|e| io(store.root(), &e))?;
            if json {
                print_json(&pruned);
            } else {
                for root in &pruned.workspaces {
                    println!("deleted workspace {}", root.display());
                }
                println!(
                    "pruned {} workspaces, {} checkpoints, {} journals; freed {} bytes",
                    pruned.workspaces.len(),
                    pruned.checkpoints,
                    pruned.journals,
                    pruned.bytes
                );
            }
            Ok(pmt_core::exit::SUCCESS)
        }
        SnapshotsCommand::Clear { force } => clear(store, *force),
    }
}

fn clear(store: &SnapshotStore, force: bool) -> Result<u8, Error> {
    if !force {
        let what = format!("delete every file snapshot in {}", store.root().display());
        match crate::prompt::confirm_typed(&what, "clear")? {
            None => {
                eprintln!("error: confirmation needed; rerun with -f");
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
            Some(false) => {
                eprintln!("nothing deleted");
                return Ok(pmt_core::exit::FAILURE);
            }
            Some(true) => {}
        }
    }
    store.clear().map_err(|e| io(store.root(), &e))?;
    println!("snapshot store cleared");
    Ok(pmt_core::exit::SUCCESS)
}

fn status_text(status: &manage::Status) -> String {
    let mut text = format!(
        "snapshot store: {} bytes, {} workspaces\n",
        status.total_bytes, status.workspace_count
    );
    for ws in &status.workspaces {
        let used = jiff::Timestamp::from_millisecond(ws.last_used_ms).map_or_else(
            |_| "?".to_owned(),
            |t| t.strftime("%Y-%m-%d %H:%M").to_string(),
        );
        let _ = writeln!(
            text,
            "  {}{}  {} bytes  {} checkpoints  last used {used}",
            ws.root.display(),
            if ws.orphan { " (missing)" } else { "" },
            ws.bytes,
            ws.checkpoints
        );
    }
    text
}
