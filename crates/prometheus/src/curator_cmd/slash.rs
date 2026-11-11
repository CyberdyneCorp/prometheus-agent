//! `/curator [status|run [--consolidate] [--dry-run]|pause|resume|pin <s>|
//! unpin <s>|archive <s>|restore <s>|ledger]` on every chat surface (spec:
//! skills, /curator slash command): the CLI's operations; `run` goes to
//! the background and posts its summary (with the rename map) to the same
//! chat when it finishes.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

use super::{Curator, ledger_text, report_text};
use crate::chat::commands::Post;

const USAGE: &str = "usage: /curator [status | run [--consolidate] [--dry-run] | pause | resume | pin <skill> | unpin <skill> | archive <skill> | restore <skill> | ledger [skill]]";

/// Runs `/curator <args>`.
///
/// # Errors
///
/// `cli.usage` and the operation's failure.
pub(crate) fn run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    args: &str,
    post: &Post,
) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let curator = Curator::open(scope.clone(), launch.clone())?.in_language();
    match words.as_slice() {
        [] | ["status"] => Ok(status(&curator)?),
        ["run", flags @ ..]
            if flags
                .iter()
                .all(|f| matches!(*f, "--consolidate" | "--dry-run")) =>
        {
            let dry_run = flags.contains(&"--dry-run");
            let consolidate = flags.contains(&"--consolidate");
            start(curator, dry_run, consolidate, post)
        }
        ["pause"] => curator.pause(true),
        ["resume"] => curator.pause(false),
        ["pin", name] => curator.pin(name, true),
        ["unpin", name] => curator.pin(name, false),
        ["archive", name] => curator.archive(name),
        ["restore", name] => curator.restore(name),
        ["ledger"] => Ok(ledger_text(&*curator.text, &curator.entries(None, 20)?)),
        ["ledger", skill] => Ok(ledger_text(
            &*curator.text,
            &curator.entries(Some(skill), 20)?,
        )),
        _ => Err(Error::new(&codes::CLI_USAGE, USAGE)),
    }
}

/// `/curator status`.
fn status(curator: &Curator) -> Result<String, Error> {
    let s = curator.status()?;
    let text = &curator.text;
    let state = text.get(if !s.enabled {
        "slash.curator.state_disabled"
    } else if s.paused {
        "slash.curator.state_paused"
    } else {
        "slash.curator.state_enabled"
    });
    let never = text.get("slash.curator.never");
    Ok(text.fmt(
        "slash.curator.status",
        &[
            ("state", &state),
            ("last", &s.last_run_at.as_deref().unwrap_or(never)),
            ("managed", &s.managed.len()),
            ("pinned", &s.pinned.len()),
            ("archived", &s.archived),
            ("planned", &s.planned.len()),
        ],
    ))
}

/// Runs the pass on its own thread and posts its report.
fn start(curator: Curator, dry_run: bool, consolidate: bool, post: &Post) -> Result<String, Error> {
    let post = Arc::clone(post);
    let started = curator.text.get(if dry_run {
        "slash.curator.dry_run_started"
    } else {
        "slash.curator.pass_started"
    });
    let started = started.to_owned();
    std::thread::Builder::new()
        .name("pmt-curator".to_owned())
        .spawn(move || {
            let text = match curator.run(dry_run, consolidate, false) {
                Ok(report) => report_text(&*curator.text, &report).trim_end().to_owned(),
                Err(e) => curator
                    .text
                    .fmt("slash.curator.failed", &[("why", &e.message())]),
            };
            post(text);
        })
        .map_err(|e| {
            Error::new(
                &codes::OUTPUT_IO,
                format!("cannot start the curator pass: {e}"),
            )
        })?;
    Ok(started)
}
