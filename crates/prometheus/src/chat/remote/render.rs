//! What an attached chat shows of a turn the daemon runs: one progress
//! line per tool call and the notices of the session's commands (from its
//! `session.event` stream), the questions it asks, the final answer and
//! its error. A one-shot run writes them plainly ([`Plain`]); the line
//! interface paints them above its prompt.

use std::collections::BTreeMap;
use std::io::Write as _;

use pmt_core::error::Error;
use pmt_rpc::contracts::{AutomationStatePayload, TurnFinished};
use serde_json::Value;

use pmt_rpc::contracts::{COMMAND_NOTICE, PROCESS_NOTICE};

/// Where an attached chat shows things.
pub(super) trait Out {
    /// A tool progress line.
    fn tool(&self, text: &str);
    /// A notice (a command's output, a status line).
    fn notice(&self, text: &str);
    /// A question to answer on the next line.
    fn ask(&self, text: &str);
    /// The assistant's final answer.
    fn answer(&self, text: &str);
    /// An error.
    fn error(&self, err: &Error);
}

/// A one-shot run: progress, notices and errors on stderr, the answer on
/// stdout; `-q` shows the answer only, `--json` no progress.
#[derive(Debug, Clone, Copy)]
pub(super) struct Plain {
    /// `-q`.
    pub quiet: bool,
    /// `--json`.
    pub json: bool,
}

impl Out for Plain {
    fn tool(&self, text: &str) {
        if !self.quiet && !self.json {
            eprintln!("{text}");
        }
    }

    fn notice(&self, text: &str) {
        if !self.quiet && !self.json {
            eprintln!("{text}");
        }
    }

    fn ask(&self, text: &str) {
        eprintln!("{text}");
    }

    fn answer(&self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = if text.ends_with('\n') {
            write!(out, "{text}")
        } else {
            writeln!(out, "{text}")
        };
        let _ = out.flush();
    }

    fn error(&self, err: &Error) {
        crate::output::print_error(err, self.json);
    }
}

/// Shows one `session.event` of the session.
pub(super) fn event(out: &dyn Out, data: &Value) {
    match data["type"].as_str() {
        Some("tool.call") => {
            let tool = data["payload"]["tool"].as_str().unwrap_or("tool");
            out.tool(&format!("· {tool}"));
        }
        Some(COMMAND_NOTICE) => out.notice(data["payload"]["text"].as_str().unwrap_or_default()),
        Some(PROCESS_NOTICE) => out.notice(&format!(
            "[background] {}",
            data["payload"]["text"].as_str().unwrap_or_default()
        )),
        _ => {}
    }
}

/// Whether a `session.event` is a notice posted between turns (a command's
/// output, a background process) rather than part of a turn.
pub(super) fn is_notice(data: &Value) -> bool {
    matches!(data["type"].as_str(), Some(COMMAND_NOTICE | PROCESS_NOTICE))
}

/// Keeps the latest live row per kind from an `automation.state` event of
/// `session` (spec: session-automation, Automation status on every
/// surface); a removed goal, loop or heartbeat drops its row.
pub(super) fn automation(rows: &mut BTreeMap<String, String>, session: &str, data: &Value) {
    let Ok(state) = serde_json::from_value::<AutomationStatePayload>(data.clone()) else {
        return;
    };
    if state.session_id != session {
        return;
    }
    match state.row {
        Some(row) => {
            rows.insert(state.kind, row);
        }
        None => {
            rows.remove(&state.kind);
        }
    }
}

/// Shows the turn's answer, then its error.
pub(super) fn finished(out: &dyn Out, finished: &TurnFinished) {
    if let Some(answer) = &finished.final_response {
        out.answer(answer);
    }
    if let Some(error) = &finished.error {
        out.error(&error.to_error());
    }
}

/// The exit status of a finished turn, as a one-shot run reports it.
#[must_use]
pub(super) fn status(finished: &TurnFinished) -> u8 {
    match finished.exit_reason.as_str() {
        "completed" => 0,
        "token_budget_exhausted"
        | "cost_budget_exhausted"
        | "run_budget_exhausted"
        | "iteration_limit" => pmt_core::exit::BUDGET_EXHAUSTED,
        "interrupted_by_user" => pmt_core::exit::INTERRUPTED,
        _ => finished
            .error
            .as_ref()
            .map_or(1, |e| e.to_error().exit_code()),
    }
}
