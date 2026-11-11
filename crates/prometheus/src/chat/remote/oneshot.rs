//! A one-shot run attached to the daemon: one turn, the answer on stdout
//! (or with `--json` one object), progress and errors on stderr, the same
//! exit statuses as an in-process run.

use std::io::Write as _;

use pmt_core::error::{Error, codes};
use serde::Serialize;

use super::render::{self, Out, Plain};
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
    if start.ui.stream {
        return streamed(attached, start).await;
    }
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

/// A streamed run (`--output-format stream-json`): the session's
/// trajectory events as JSON lines on stdout, then the `result` line;
/// errors as JSON on stderr.
struct Streamed {
    session_id: String,
    redactor: Option<pmt_core::secret::Redactor>,
    seq: std::sync::atomic::AtomicU64,
    /// The newest daemon sequence number written (an event replayed at
    /// the start and delivered live again is written once).
    seen: std::sync::atomic::AtomicU64,
}

impl Out for Streamed {
    fn tool(&self, _: &str) {}

    fn notice(&self, _: &str) {}

    fn ask(&self, text: &str) {
        eprintln!("{text}");
    }

    fn answer(&self, _: &str) {}

    fn raw_event(&self, event: &pmt_rpc::contracts::Envelope) {
        use std::sync::atomic::Ordering;
        if event.seq <= self.seen.fetch_max(event.seq, Ordering::Relaxed) {
            return;
        }
        let data = &event.data;
        let Some(kind) = data["type"].as_str() else {
            return;
        };
        // Only the trajectory grammar: not the clients' own deltas and
        // command notices.
        if !pmt_obs::trajectory::events::EVENT_TYPES.contains(&kind) {
            return;
        }
        let payload = match &self.redactor {
            Some(redactor) => redactor.redact_json(data["payload"].clone()),
            None => data["payload"].clone(),
        };
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let text = |key: &str| data[key].as_str().map(str::to_owned);
        crate::chat::stream::write_line(&pmt_obs::trajectory::Line {
            v: pmt_obs::trajectory::version(),
            ts: text("ts").unwrap_or_default(),
            seq,
            session_id: self.session_id.clone(),
            // The daemon's events do not name the lineage root (no RPC
            // contract carries it): the line names the session itself.
            root_session_id: self.session_id.clone(),
            turn_id: text("turn_id"),
            span_id: text("span_id").unwrap_or_default(),
            parent_span_id: text("parent_span_id"),
            kind: kind.to_owned(),
            payload,
        });
    }

    fn error(&self, err: &Error) {
        crate::output::print_error(err, true);
    }
}

async fn streamed(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    use crate::chat::stream::{result_line, write_line};
    let out = Streamed {
        session_id: attached.session.clone(),
        redactor: start.scope.secrets().redactor().ok(),
        seq: std::sync::atomic::AtomicU64::new(0),
        seen: std::sync::atomic::AtomicU64::new(0),
    };
    // What the session recorded before this client followed it (its
    // `session.start`).
    let since = pmt_rpc::contracts::EventsSinceParams {
        session_id: attached.session.clone(),
        last_seen: 0,
    };
    if let Ok(replayed) = attached
        .client
        .call_typed::<pmt_rpc::contracts::SessionEventsSince>(&since)
        .await
    {
        for event in replayed
            .events
            .iter()
            .filter(|e| e.event == "session.event")
        {
            out.raw_event(event);
        }
    }
    let mut signals = Signals::listen();
    let mut asker = Asker::OneShot {
        approve: start.approve_unattended,
    };
    let prompt = Prompt {
        text: start.first.clone().unwrap_or_default(),
        images: start.first_images(),
    };
    let outcome = turn::run(attached, prompt, &mut asker, &out, &mut signals).await?;
    let null = serde_json::Value::Null;
    if outcome.approval_needed {
        let err = Error::new(&codes::CHAT_APPROVAL_REQUIRED, APPROVAL_MESSAGE);
        crate::output::print_error(&err, true);
        write_line(&result_line(
            &attached.session,
            None,
            &null,
            "approval_required",
        ));
        return Ok(pmt_core::exit::APPROVAL_REQUIRED);
    }
    let Some(finished) = outcome.finished else {
        write_line(&result_line(
            &attached.session,
            None,
            &null,
            "interrupted_by_user",
        ));
        return Ok(outcome.signal.unwrap_or(1));
    };
    write_line(&result_line(
        &attached.session,
        finished.final_response.as_deref(),
        finished.usage.as_ref().unwrap_or(&null),
        &finished.exit_reason,
    ));
    if let Some(error) = &finished.error {
        crate::output::print_error(&error.to_error(), true);
    }
    Ok(outcome.signal.unwrap_or_else(|| render::status(&finished)))
}
