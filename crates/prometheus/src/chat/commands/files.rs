//! `/undo`, `/rollback` and `/diff` (spec: execution-environments, File
//! snapshots and undo; Rollback listing, diff and selective restore;
//! Working-tree diff command), shared with `prometheus files` and the
//! `files.undo` / `files.rollback` RPC methods.

use std::fmt::Write as _;
use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::snapshots::restore::{self, UndoReport};
use pmt_tools::snapshots::{HostTree, SnapshotStore, Tree, worktree};

use super::Ctx;
use crate::slash::Surface;

fn io(store: &SnapshotStore, err: &std::io::Error) -> Error {
    Error::io(&codes::HOME_IO, store.root(), err)
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// Appends the conversation note recording an undo (earlier messages are
/// never rewritten).
///
/// # Errors
///
/// Store errors.
pub fn note_undo(store: &Store, session: SessionId, report: &UndoReport) -> Result<(), Error> {
    let files: Vec<String> = report
        .restored
        .iter()
        .chain(&report.removed)
        .map(|p| p.display().to_string())
        .collect();
    let text = format!(
        "[The user undid the file changes of turn(s) {:?}: {} now hold their earlier content.]",
        report.turns,
        files.join(", ")
    );
    store.append_message(
        session,
        &pmt_agent::turn::transcript::notice_row(&text, "files_undo"),
    )?;
    Ok(())
}

/// Undoes the file changes of `live`'s last `count` turns (nothing when
/// its snapshots are off) and notes it in the conversation.
///
/// # Errors
///
/// Store errors, a file that cannot be restored.
pub fn undo_files(
    store: &Store,
    live: &crate::chat::session::Live,
    count: u64,
    force: bool,
    dry_run: bool,
) -> Result<UndoReport, Error> {
    let Some(snapshots) = &live.place.snapshots() else {
        return Ok(UndoReport::default());
    };
    let turn = store.user_turns(live.id)?;
    let report = restore::undo(
        snapshots.store(),
        snapshots.tree(),
        &live.id.to_string(),
        Some(turn),
        count,
        force,
        dry_run,
    )
    .map_err(|e| io(snapshots.store(), &e))?;
    if !dry_run && report.conflicts.is_empty() && !report.turns.is_empty() {
        note_undo(store, live.id, &report)?;
    }
    Ok(report)
}

/// `/undo [n] [--force]`: the conversation's last exchange and the file
/// changes of its last `n` turns.
///
/// # Errors
///
/// Store errors, an invalid count.
pub fn undo(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut count = 1;
    let mut force = false;
    for word in args.split_whitespace() {
        match word {
            "--force" => force = true,
            n => {
                count = n
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| invalid(format!("/undo takes a turn count, not '{n}'")))?;
            }
        }
    }
    let id = ctx.live.id;
    let mut text = String::new();
    let report = undo_files(&ctx.factory.store, ctx.live, count, force, false)?;
    if !report.conflicts.is_empty() {
        return Ok(crate::files_cmd::undo_text(&report, false));
    }
    if !report.turns.is_empty() {
        text.push_str(&crate::files_cmd::undo_text(&report, false));
    }
    let mut messages = 0;
    for _ in 0..count {
        match ctx.factory.store.undo_last_turn(id, None) {
            Ok(rewound) => messages += rewound.messages.len(),
            Err(_) if messages > 0 => break,
            Err(err) => return Err(err),
        }
    }
    text.push_str(&pmt_i18n::t!(
        ctx.strings(),
        "slash.files.undid",
        count = messages
    ));
    Ok(text)
}

/// `/rollback [diff] [N] [<file>] [--all]`.
///
/// # Errors
///
/// No snapshots for this session, an unknown checkpoint.
pub fn rollback(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let all = remove(&mut words, "--all");
    let diff = words.first() == Some(&"diff");
    if diff {
        words.remove(0);
    }
    let number = words
        .first()
        .map(|w| {
            w.parse::<usize>()
                .map_err(|_| invalid(format!("/rollback takes a checkpoint number, not '{w}'")))
        })
        .transpose()?;
    rollback_live(
        &ctx.factory.store,
        ctx.live,
        number,
        words.get(1).copied(),
        diff,
        all,
    )
}

/// The rollback listing, diff or restore of `live`'s workspace; a restore
/// appends a note marking the latest turn as undone.
///
/// # Errors
///
/// An unknown checkpoint, store I/O.
pub fn rollback_live(
    store: &Store,
    live: &crate::chat::session::Live,
    number: Option<usize>,
    file: Option<&str>,
    diff: bool,
    all: bool,
) -> Result<String, Error> {
    let strings = pmt_config::language::strings(live.scope.dir(), pmt_i18n::Surface::Core);
    let Some(snapshots) = &live.place.snapshots() else {
        return Ok(pmt_i18n::t!(strings, "slash.files.snapshots_off"));
    };
    let reply = rollback_reply(
        &strings,
        snapshots.store(),
        snapshots.tree(),
        snapshots.workspace(),
        number,
        file,
        diff,
        all,
    )?;
    if number.is_some() && !diff {
        let note = "[The user rolled the workspace back to an earlier checkpoint; the latest turn's file changes are undone.]";
        store.append_message(
            live.id,
            &pmt_agent::turn::transcript::notice_row(note, "files_rollback"),
        )?;
    }
    Ok(reply)
}

/// The listing, the diff or the restore of checkpoint `number` of the
/// workspace holding `dir` in `tree`.
///
/// # Errors
///
/// An unknown checkpoint, store I/O.
#[allow(clippy::too_many_arguments)] // the rollback's own flags, plus where and in which language
pub fn rollback_reply(
    strings: &pmt_i18n::Strings,
    store: &SnapshotStore,
    tree: &dyn Tree,
    dir: &Path,
    number: Option<usize>,
    file: Option<&str>,
    diff: bool,
    all: bool,
) -> Result<String, Error> {
    let checkpoints = restore::checkpoints(store, tree, dir);
    let Some(number) = number else {
        if checkpoints.is_empty() {
            return Ok(pmt_i18n::t!(strings, "slash.files.no_checkpoints"));
        }
        let mut text = format!("{}\n", strings.get("slash.files.checkpoints"));
        for row in restore::listing(store, tree, &checkpoints) {
            let at = jiff::Timestamp::from_millisecond(row.created_ms).map_or_else(
                |_| "?".to_owned(),
                |t| t.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            );
            let line = pmt_i18n::t!(
                strings,
                "slash.files.checkpoint_row",
                n = row.number,
                at = at,
                tool = row.tool,
                files = row.files_changed,
                lines = row.lines_changed
            );
            let _ = writeln!(text, "  {line}");
        }
        return Ok(text);
    };
    let checkpoint = checkpoints.get(number.wrapping_sub(1)).ok_or_else(|| {
        invalid(format!(
            "no checkpoint {number}; there are {}",
            checkpoints.len()
        ))
    })?;
    if diff {
        return Ok(restore::diff(store, tree, checkpoint));
    }
    let report =
        restore::rollback(store, tree, checkpoint, all, file).map_err(|e| io(store, &e))?;
    Ok(report.render())
}

fn remove(words: &mut Vec<&str>, flag: &str) -> bool {
    let before = words.len();
    words.retain(|w| *w != flag);
    words.len() != before
}

/// `/diff [unstaged|staged|all|session] [--stat] [<path>...]`; on a
/// messaging platform fenced and cut to its message limit.
#[must_use]
pub fn diff(ctx: &Ctx<'_>, args: &str) -> String {
    let request = worktree::Request::parse(args);
    let host = SnapshotStore::of_profile(ctx.scope.dir());
    let snapshots = ctx.live.place.snapshots();
    let (store, tree): (&SnapshotStore, &dyn Tree) = match &snapshots {
        Some(s) => (s.store(), s.tree()),
        None => (&host, &HostTree),
    };
    let text = worktree::diff(
        store,
        tree,
        &ctx.live.place.worktree(),
        &ctx.live.id.to_string(),
        &request,
    );
    match (ctx.surface, ctx.message_limit) {
        (Surface::Gateway, limit) => worktree::for_chat(&text, limit.unwrap_or(4000)),
        _ => text,
    }
}
