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
    let curator = Curator::open(scope.clone(), launch.clone())?;
    match words.as_slice() {
        [] | ["status"] => {
            let s = curator.status()?;
            Ok(format!(
                "Curator {}; last pass {}; managing {} skill(s), {} pinned, {} archived; a pass now would make {} transition(s).",
                if !s.enabled {
                    "disabled"
                } else if s.paused {
                    "paused"
                } else {
                    "enabled"
                },
                s.last_run_at.as_deref().unwrap_or("never"),
                s.managed.len(),
                s.pinned.len(),
                s.archived,
                s.planned.len()
            ))
        }
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
        ["ledger"] => Ok(ledger_text(&curator.entries(None, 20)?)),
        ["ledger", skill] => Ok(ledger_text(&curator.entries(Some(skill), 20)?)),
        _ => Err(Error::new(&codes::CLI_USAGE, USAGE)),
    }
}

/// Runs the pass on its own thread and posts its report.
fn start(curator: Curator, dry_run: bool, consolidate: bool, post: &Post) -> Result<String, Error> {
    let post = Arc::clone(post);
    std::thread::Builder::new()
        .name("pmt-curator".to_owned())
        .spawn(move || {
            let text = match curator.run(dry_run, consolidate, false) {
                Ok(report) => report_text(&report).trim_end().to_owned(),
                Err(e) => format!("Curator pass failed: {}", e.message()),
            };
            post(text);
        })
        .map_err(|e| {
            Error::new(
                &codes::OUTPUT_IO,
                format!("cannot start the curator pass: {e}"),
            )
        })?;
    Ok(format!(
        "Curator {} started in the background; its summary is posted here when it finishes.",
        if dry_run { "dry run" } else { "pass" }
    ))
}
