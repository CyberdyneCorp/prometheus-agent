//! The line interface attached to the daemon: prompts become turns of the
//! daemon's session, lines typed while a turn runs wait their turn, Ctrl-C
//! interrupts a running turn (twice at the prompt leaves), and the slash
//! commands whose state lives in the daemon go through the connection.

use std::collections::VecDeque;
use std::io::Write as _;
use std::time::Instant;

use pmt_core::error::Error;
use pmt_rpc::client::error_of;
use pmt_rpc::contracts::{
    CompactParams, SessionCompact, SessionCreate, SessionCreateParams, SessionDetach, SessionRef,
};
use tokio::sync::mpsc;

use super::render::View;
use super::turn::{self, Asker};
use super::{Attached, Start};
use crate::chat::repl::DOUBLE_CTRL_C;
use crate::chat::signals::Signals;

/// Lines typed at the terminal, read on a thread of their own so the
/// interface keeps rendering while it waits.
pub(super) struct Input {
    lines: mpsc::UnboundedReceiver<String>,
    /// Lines typed while a turn ran, run after it in order.
    pub queue: VecDeque<String>,
    /// Stdin reached its end.
    pub closed: bool,
}

impl Input {
    fn spawn() -> Self {
        let (tx, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        let text = line.trim_end_matches(['\n', '\r']).to_owned();
                        if tx.send(text).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            lines,
            queue: VecDeque::new(),
            closed: false,
        }
    }

    /// The next typed line; `None` at the end of input.
    pub(super) async fn next(&mut self) -> Option<String> {
        self.lines.recv().await
    }
}

/// What the loop does after a line.
enum Flow {
    Continue,
    Exit,
}

/// Runs the attached line interface until `/exit` or the end of input.
///
/// # Errors
///
/// A lost connection.
pub(super) async fn run(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    let view = View {
        quiet: start.ui.quiet,
        json: false,
    };
    let mut input = Input::spawn();
    let mut signals = Signals::listen();
    println!(
        "attached to the gateway daemon (pid {}) · session {}",
        attached.daemon_pid, attached.session
    );
    println!("type /help for the commands available here, /exit to leave");
    if let Some(first) = &start.first {
        run_turn(attached, first.clone(), &mut input, view, &mut signals).await?;
    }
    let mut last_interrupt: Option<Instant> = None;
    loop {
        let line = if let Some(line) = input.queue.pop_front() {
            Some(line)
        } else {
            prompt();
            tokio::select! {
                line = input.next() => line,
                _ = signals.next() => {
                    if last_interrupt.is_some_and(|t| t.elapsed() < DOUBLE_CTRL_C) {
                        return Ok(0);
                    }
                    last_interrupt = Some(Instant::now());
                    println!("\n(press Ctrl+C again to leave)");
                    continue;
                }
            }
        };
        let Some(line) = line else {
            println!();
            return Ok(0);
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with('/') {
            if let Flow::Exit = slash(attached, text).await? {
                return Ok(0);
            }
            continue;
        }
        run_turn(attached, line.clone(), &mut input, view, &mut signals).await?;
    }
}

fn prompt() {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "› ");
    let _ = out.flush();
}

async fn run_turn(
    attached: &mut Attached,
    text: String,
    input: &mut Input,
    view: View,
    signals: &mut Signals,
) -> Result<(), Error> {
    let mut asker = Asker::Line(input);
    match turn::run(attached, text, &mut asker, view, signals).await {
        Ok(outcome) => {
            if let Some(finished) = &outcome.finished {
                view.finished(finished);
                if finished.exit_reason == "interrupted_by_user" {
                    println!("(interrupted)");
                }
            }
            Ok(())
        }
        // A refused submission (a busy or over-budget session) leaves the
        // interface running; a lost connection ends it.
        Err(err) if err.code().code == pmt_core::error::codes::RPC_CONNECTION.code => Err(err),
        Err(err) => {
            crate::output::print_error(&err, false);
            Ok(())
        }
    }
}

/// The commands an attached chat runs. Every other registered command
/// needs the session in this process.
const HELP: &str = "commands while attached to the gateway daemon:
  /new (/reset)        start a new session
  /fork                fork this session and continue in the fork
  /compress [focus]    compact this session's context now (/compact)
  /help                this list
  /exit (/quit)        leave (the session stays in the daemon)
Ctrl+C interrupts a running turn; twice at the prompt leaves.";

async fn slash(attached: &mut Attached, text: &str) -> Result<Flow, Error> {
    let (name, rest) = text[1..].split_once(' ').unwrap_or((&text[1..], ""));
    match name {
        "exit" | "quit" => return Ok(Flow::Exit),
        "help" => println!("{HELP}"),
        "new" | "reset" => switch(attached, new_session(attached).await).await,
        "fork" => {
            let fork = super::open::fork(&attached.client, &attached.session).await;
            switch(attached, fork).await;
        }
        "compress" | "compact" => compact(attached, rest.trim()).await,
        _ => println!("{}", unavailable(name, attached.daemon_pid)),
    }
    Ok(Flow::Continue)
}

fn unavailable(name: &str, daemon_pid: u32) -> String {
    let known = crate::slash::builtins::BUILTINS
        .iter()
        .any(|c| c.name == name || c.aliases.contains(&name));
    if known {
        format!(
            "/{name} needs the session in this process; it is not available while attached to the gateway daemon (pid {daemon_pid}). Stop the daemon with 'prometheus daemon stop' to use it."
        )
    } else {
        format!("unknown command '/{name}' (see /help)")
    }
}

async fn new_session(attached: &Attached) -> Result<String, Error> {
    let params = SessionCreateParams {
        exclusive: true,
        ..SessionCreateParams::default()
    };
    attached
        .client
        .call_typed::<SessionCreate>(&params)
        .await
        .map(|created| created.session_id)
        .map_err(|e| error_of(&e))
}

/// Leaves the current session for `next` (or reports why not).
async fn switch(attached: &mut Attached, next: Result<String, Error>) {
    match next {
        Ok(next) => {
            let old = SessionRef {
                session_id: std::mem::replace(&mut attached.session, next),
            };
            let _ = attached.client.call_typed::<SessionDetach>(&old).await;
            println!("session {}", attached.session);
        }
        Err(err) => crate::output::print_error(&err, false),
    }
}

async fn compact(attached: &Attached, focus: &str) {
    let params = CompactParams {
        session_id: attached.session.clone(),
        focus: (!focus.is_empty()).then(|| focus.to_owned()),
    };
    match attached.client.call_typed::<SessionCompact>(&params).await {
        Ok(done) => println!(
            "compaction {}: {} -> {} tokens",
            done.status, done.tokens_before, done.tokens_after
        ),
        Err(err) => crate::output::print_error(&error_of(&err), false),
    }
}

/// Follows a session another client drives, read-only: its turns are
/// shown as they run until Ctrl-C or the end of input.
///
/// # Errors
///
/// `session.attach` errors.
pub(super) async fn follow(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    let params = SessionRef {
        session_id: attached.session.clone(),
    };
    attached
        .client
        .call_typed::<pmt_rpc::contracts::SessionAttach>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    println!(
        "following session {} read-only; Ctrl+C leaves",
        attached.session
    );
    let view = View {
        quiet: start.ui.quiet,
        json: false,
    };
    let mut signals = Signals::listen();
    loop {
        tokio::select! {
            event = attached.inbox.events.recv() => {
                let Some(event) = event else { return Ok(0) };
                show_followed(&event, view);
            }
            _ = signals.next() => return Ok(0),
        }
    }
}

fn show_followed(event: &pmt_rpc::contracts::Envelope, view: View) {
    match event.event.as_str() {
        "session.event" if event.data["type"] == "user.message" => {
            let text = event.data["payload"]["text"].as_str().unwrap_or_default();
            println!("› {text}");
        }
        "session.event" => view.event(&event.data),
        "turn.finished" => {
            if let Ok(finished) = serde_json::from_value(event.data.clone()) {
                view.finished(&finished);
            }
        }
        _ => {}
    }
}
