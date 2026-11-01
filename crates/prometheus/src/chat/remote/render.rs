//! What an attached chat shows of a turn the daemon runs: one progress
//! line per tool call (from the session's `session.event` stream) on
//! stderr, and the final answer on stdout.

use std::io::Write as _;

use pmt_rpc::contracts::TurnFinished;
use serde_json::Value;

/// How much the chat shows.
#[derive(Debug, Clone, Copy)]
pub(super) struct View {
    /// `-q`: the answer only.
    pub quiet: bool,
    /// `--json`: no progress lines.
    pub json: bool,
}

impl View {
    /// Shows one trajectory event of the session.
    pub(super) fn event(self, data: &Value) {
        if self.quiet || self.json {
            return;
        }
        if data["type"] == "tool.call" {
            let tool = data["payload"]["tool"].as_str().unwrap_or("tool");
            eprintln!("· {tool}");
        }
    }

    /// Prints the turn's answer, and its error on stderr.
    pub(super) fn finished(self, finished: &TurnFinished) {
        if let Some(answer) = &finished.final_response {
            let mut out = std::io::stdout().lock();
            let _ = if answer.ends_with('\n') {
                write!(out, "{answer}")
            } else {
                writeln!(out, "{answer}")
            };
            let _ = out.flush();
        }
        if let Some(error) = &finished.error {
            crate::output::print_error(&error.to_error(), self.json);
        }
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
