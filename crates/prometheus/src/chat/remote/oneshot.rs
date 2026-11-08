//! A one-shot run attached to the daemon: one turn, the answer on stdout
//! (or with `--json` one object), progress and errors on stderr, the same
//! exit statuses as an in-process run.

use std::io::Write as _;

use pmt_core::error::{Error, codes};
use serde::Serialize;

use super::render::{self, Plain};
use super::turn::{self, Asker, Prompt};
use super::{Attached, Start};
use crate::chat::oneshot::APPROVAL_MESSAGE;
use crate::chat::signals::Signals;

/// The `--json` document, as an in-process one-shot run prints it.
#[derive(Debug, Serialize)]
struct Report<'a> {
    session_id: &'a str,
    answer: Option<&'a str>,
    usage: Option<&'a serde_json::Value>,
    exit_reason: &'a str,
}

/// Runs the prompt as the session's one turn; returns the exit status.
///
/// # Errors
///
/// The submission's refusal and a lost connection.
pub(super) async fn run(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    let json = start.ui.json;
    let out = Plain {
        quiet: start.ui.quiet,
        json,
    };
    let mut signals = Signals::listen();
    let mut asker = Asker::OneShot {
        approve: start.approve_unattended,
    };
    let prompt = Prompt {
        text: start.first.clone().unwrap_or_default(),
        images: start.first_images(),
    };
    let outcome = turn::run(attached, prompt, &mut asker, &out, &mut signals).await?;
    if outcome.approval_needed {
        let err = Error::new(&codes::CHAT_APPROVAL_REQUIRED, APPROVAL_MESSAGE);
        crate::output::print_error(&err, json);
        return Ok(pmt_core::exit::APPROVAL_REQUIRED);
    }
    let Some(finished) = outcome.finished else {
        return Ok(outcome.signal.unwrap_or(1));
    };
    // The session ends with the run (spec: memory, Reviewable memory
    // changes per session).
    if let Some(line) =
        crate::daemon_client::memory_summary(&attached.client, &attached.session).await
    {
        eprintln!("{line}");
    }
    if json {
        let report = Report {
            session_id: &attached.session,
            answer: finished.final_response.as_deref(),
            usage: finished.usage.as_ref(),
            exit_reason: &finished.exit_reason,
        };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        );
        drop(out);
        if let Some(error) = &finished.error {
            crate::output::print_error(&error.to_error(), true);
        }
    } else {
        render::finished(&out, &finished);
    }
    Ok(outcome.signal.unwrap_or_else(|| render::status(&finished)))
}
