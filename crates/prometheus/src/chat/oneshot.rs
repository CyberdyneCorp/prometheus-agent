//! One-shot and piped mode (spec: cli, One-shot and piped mode; Stable
//! exit codes): one turn, only the final answer on stdout (or with
//! `--json` one object), progress and errors on stderr, approvals nobody
//! can give end the run with exit 4.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use pmt_agent::turn::{ExitReason, Interrupt, Issuer, TurnInput, TurnOptions, TurnResult};
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use serde::Serialize;

use super::args::PromptSource;
use super::session::Live;

/// Exit status when a call needed an approval nobody could give.
pub const NEEDS_APPROVAL: u8 = 4;
/// Exit status of a run that ran out of budget before a final answer.
pub const BUDGET_EXHAUSTED: u8 = 3;
/// SIGINT.
pub const INTERRUPTED: u8 = 130;
/// SIGTERM.
pub const TERMINATED: u8 = 143;

/// The message of a run that stopped on an approval.
pub const APPROVAL_MESSAGE: &str = "a tool call needed an approval, which a one-shot run cannot ask for; run interactively, or allow it with --approvals off or approvals.single_query_mode = \"approve\"";

/// The prompt of a one-shot run: the flag text, `--prompt-file` or stdin.
/// With text on the flag, piped stdin is appended as its context
/// (`cat notes.md | prometheus -z "summarise"`). Bytes are taken
/// verbatim (decoded as UTF-8).
///
/// # Errors
///
/// `chat.prompt_file` (66) for a missing file, `chat.empty_prompt` (2).
pub fn prompt(source: &PromptSource) -> Result<String, Error> {
    let text = match source {
        PromptSource::Text(text) => {
            let piped = piped_stdin()?;
            if piped.trim().is_empty() {
                text.clone()
            } else {
                format!("{text}\n\n{}", piped.trim_end_matches(['\n', '\r']))
            }
        }
        PromptSource::File(path) => read_prompt_file(path)?,
        // `echo hi |` ends the prompt with the shell's newline.
        PromptSource::None => piped_stdin()?.trim_end_matches(['\n', '\r']).to_owned(),
    };
    if text.trim().is_empty() {
        return Err(Error::new(
            &codes::CHAT_EMPTY_PROMPT,
            "no prompt: pass -z <prompt>, --prompt-file <path> or pipe it on stdin",
        ));
    }
    Ok(text)
}

/// `--prompt-file <path>` (`-` reads stdin).
///
/// # Errors
///
/// `chat.prompt_file` (66).
pub fn read_prompt_file(path: &Path) -> Result<String, Error> {
    if path == Path::new("-") {
        return piped_stdin();
    }
    let bytes = std::fs::read(path).map_err(|e| {
        Error::new(
            &codes::CHAT_PROMPT_FILE,
            format!("cannot read --prompt-file {}: {e}", path.display()),
        )
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Stdin when it is not a terminal (empty for a terminal).
fn piped_stdin() -> Result<String, Error> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(String::new());
    }
    let mut bytes = Vec::new();
    stdin
        .read_to_end(&mut bytes)
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot read stdin: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The `--json` document of a one-shot run.
#[derive(Debug, Serialize)]
struct Report<'a> {
    session_id: String,
    answer: Option<&'a str>,
    usage: &'a pmt_agent::turn::TurnUsage,
    exit_reason: &'a str,
}

/// Runs `prompt` as the session's one turn and prints the outcome;
/// returns the exit status.
pub async fn run(live: &Live, input: TurnInput, json: bool, run_budget: Option<Duration>) -> u8 {
    let mut options = TurnOptions::new(&CancelToken::new());
    options.run_budget = run_budget;
    let interrupt = options.interrupt.clone();
    live.record.turn();
    let turn = Box::pin(
        live.agent
            .run_turn(Some(&live.scope), live.id, input, options),
    );
    let (result, signal) = drive(live, turn, &interrupt).await;
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            crate::output::print_error(&err, json);
            return err.exit_code();
        }
    };
    if live.gate.unobtainable() > 0 {
        let err = Error::new(&codes::CHAT_APPROVAL_REQUIRED, APPROVAL_MESSAGE);
        print_message(&err, json);
        return NEEDS_APPROVAL;
    }
    report(live, &result, json);
    signal.unwrap_or_else(|| status(&result))
}

/// Awaits `turn`, stopping it on SIGINT/SIGTERM or as soon as the gate
/// denies a call for want of an approval. Returns the signal's status too.
async fn drive<F>(
    live: &Live,
    turn: F,
    interrupt: &Interrupt,
) -> (Result<TurnResult, Error>, Option<u8>)
where
    F: std::future::Future<Output = Result<TurnResult, Error>>,
{
    let mut turn = std::pin::pin!(turn);
    let mut signal = None;
    let mut stopped = false;
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            result = &mut turn => return (result, signal),
            code = super::signals::next(), if signal.is_none() => {
                signal = Some(code);
                interrupt.stop(Issuer::User);
            }
            _ = tick.tick(), if !stopped => {
                if live.gate.unobtainable() > 0 {
                    stopped = true;
                    interrupt.stop(Issuer::System("approval_required".to_owned()));
                }
            }
        }
    }
}

fn report(live: &Live, result: &TurnResult, json: bool) {
    let mut out = std::io::stdout().lock();
    if json {
        let body = Report {
            session_id: live.id.to_string(),
            answer: result.final_response.as_deref(),
            usage: &result.usage,
            exit_reason: result.exit_reason.as_str(),
        };
        let _ = writeln!(out, "{}", serde_json::to_string(&body).unwrap_or_default());
        return;
    }
    if let Some(answer) = &result.final_response {
        let _ = if answer.ends_with('\n') {
            write!(out, "{answer}")
        } else {
            writeln!(out, "{answer}")
        };
    }
    if let Some(err) = &result.error {
        crate::output::print_error(err, false);
    }
}

fn print_message(err: &Error, json: bool) {
    if json {
        crate::output::print_error(err, true);
    } else {
        eprintln!("error: {}", err.message());
    }
}

/// The exit status of a finished turn.
#[must_use]
pub fn status(result: &TurnResult) -> u8 {
    match result.exit_reason {
        ExitReason::Completed => 0,
        ExitReason::TokenBudgetExhausted
        | ExitReason::CostBudgetExhausted
        | ExitReason::RunBudgetExhausted
        | ExitReason::IterationLimit => BUDGET_EXHAUSTED,
        ExitReason::InterruptedByUser => INTERRUPTED,
        _ => result.error.as_ref().map_or(1, Error::exit_code),
    }
}
