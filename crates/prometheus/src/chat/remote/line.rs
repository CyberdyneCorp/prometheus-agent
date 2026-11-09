//! The line interface attached to the daemon (spec: cli, Daemon
//! attachment with in-process fallback; Interactive input handling): the
//! same line editor as the in-process interface ([`Input`]), prompts that
//! become turns of the daemon's session, lines typed while a turn runs
//! that wait their turn, and Ctrl-C that interrupts a running turn (twice
//! at the prompt leaves). Turns the daemon starts on its own between this
//! client's turns (a goal continuation, a loop wakeup, a heartbeat) are
//! shown as they run and Ctrl-C interrupts them too; the session's live
//! automation rows join the status line (spec: session-automation,
//! Automation status on every surface; Loop stop conditions).
//!
//! Slash commands that change what this terminal shows or which session
//! it drives run here ([`super::local`]); every other command goes to the
//! daemon's `command.dispatch`, which runs it on the session and answers
//! with its output, a message to send, or why it did not run.

use std::collections::VecDeque;
use std::time::Instant;

use pmt_config::schema::MarkdownMode;
use pmt_core::error::Error;
use pmt_rpc::client::error_of;
use pmt_rpc::contracts::{CommandDispatch, DispatchParams, Dispatched, Envelope, SessionRef};

use super::render::{self, Out};
use super::turn::{self, Asker, Prompt};
use super::{Attached, Start};
use crate::chat::display::Display;
use crate::chat::markdown;
use crate::chat::repl::DOUBLE_CTRL_C;
use crate::chat::repl::input::{Event, Input, Printer, Setup};
use crate::chat::signals::Signals;
use crate::chat::theme::Role;

/// What the person did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Typed {
    /// A line.
    Line(String),
    /// Ctrl+C.
    Interrupt,
    /// The end of input.
    Eof,
    /// Ctrl+X Ctrl+E with the buffer so far.
    Compose(String),
    /// The voice record key (voice mode on).
    Record,
}

/// The line editor and what waits to run.
pub(super) struct Lines {
    editor: Input,
    /// The prompt each read shows.
    prompt: String,
    /// Lines typed while a turn ran, run after it in order.
    pub queue: VecDeque<String>,
    /// Input reached its end.
    pub closed: bool,
}

impl Lines {
    fn start(start: &Start<'_>) -> Result<Self, Error> {
        let scope = start.scope;
        let editor = Input::start(Setup {
            history: scope.dir().join("cli_history"),
            cwd: start.cwd.to_path_buf(),
            redactor: scope.secrets().redactor().unwrap_or_default(),
            vim_mode: scope.config().display.vim_mode,
            record_key: crate::chat::repl::input::editor_key(&scope.config().voice.record_key.0),
        })?;
        Ok(Self {
            editor,
            prompt: "› ".to_owned(),
            queue: VecDeque::new(),
            closed: false,
        })
    }

    /// What the person does next.
    pub(super) async fn next(&mut self) -> Typed {
        loop {
            self.editor.request(&self.prompt);
            return match self.editor.next().await {
                Event::Line(line) => Typed::Line(line),
                Event::Interrupt => Typed::Interrupt,
                Event::Eof => {
                    self.closed = true;
                    Typed::Eof
                }
                Event::Compose(buffer) => Typed::Compose(buffer),
                Event::SuspendRefused => continue,
                Event::Record => Typed::Record,
            };
        }
    }

    /// Waits until everything printed so far is out.
    pub(super) fn flush(&self) {
        self.editor.flush();
    }

    /// The bottom status line drawn before each read.
    pub(super) fn set_status(&self, text: Option<String>) {
        self.editor.set_status(text);
    }

    /// `/redraw`: clears the screen for the next read to repaint.
    pub(super) fn redraw(&self) {
        self.editor.redraw();
    }

    /// Arms the voice record key.
    pub(super) fn set_voice(&self, on: bool) {
        self.editor.set_voice(on);
    }

    /// Redraws the bottom status line at once (the voice level meter).
    pub(super) fn status_line(&self) -> crate::chat::repl::input::StatusLine {
        self.editor.status_line()
    }
}

/// The attached interface's output: painted with the profile's theme,
/// above the prompt.
pub(super) struct Screen {
    printer: Printer,
    pub(super) display: Display,
    markdown: MarkdownMode,
    scope: pmt_config::ProfileScope,
}

impl Screen {
    pub(super) fn say(&self, role: Role, text: &str) {
        self.printer.print(&self.display.paint(role, text));
    }

    /// Writes `text` as it is (control sequences included).
    pub(super) fn raw(&self, text: &str) {
        self.printer.print(text);
    }
}

impl Out for Screen {
    fn tool(&self, text: &str) {
        if !self.display.quiet && self.display.progress != pmt_config::schema::ToolProgress::Off {
            self.say(Role::Tool, text);
        }
    }

    fn notice(&self, text: &str) {
        if !text.is_empty() {
            self.say(Role::Notice, text);
        }
    }

    fn ask(&self, text: &str) {
        self.say(Role::Notice, text);
    }

    fn answer(&self, text: &str) {
        let text = match self.markdown {
            MarkdownMode::Render => markdown::render(text, self.display.color),
            MarkdownMode::Raw => text.to_owned(),
        };
        self.say(Role::Assistant, &self.display.stamped(&self.scope, &text));
    }

    fn error(&self, err: &Error) {
        let text = crate::output::render_error(err, false);
        self.say(Role::Error, text.trim_end());
    }
}

/// The attached interface.
pub(super) struct Chat<'a, 'b> {
    pub(super) attached: &'a mut Attached,
    pub(super) start: &'a Start<'b>,
    /// Declared before `lines`: fields drop in order, and the editor's
    /// printer thread ends only once the screen's printer is gone.
    pub(super) screen: Screen,
    pub(super) lines: Lines,
    pub(super) signals: Signals,
    /// `/image` attachments for the next prompt (absolute paths).
    pub(super) images: Vec<std::path::PathBuf>,
    /// `cli.confirm_destructive`.
    pub(super) confirm_destructive: bool,
    /// The bottom status line's toggles (`/statusbar`, `/battery`).
    pub(super) status_bar: crate::chat::status_bar::StatusBar,
    /// The session's model, for the status line.
    pub(super) model: Option<String>,
    /// A turn the daemon started on its own runs (seen from its events
    /// until its `turn.finished`).
    pub(super) daemon_turn: bool,
    /// The low-disk prune question waits for `on`, `off` or `later`.
    prune: bool,
    /// `/voice` and the voice conversations of this terminal.
    pub(super) voice: crate::chat::voice::VoiceClient,
    /// Speaks the answer of the turn being run (a voice turn with spoken
    /// replies on).
    pub(super) mouth: Option<std::sync::Arc<pmt_voice::speaker::Mouth>>,
}

/// What the loop does after a line.
pub(super) enum Flow {
    Continue,
    Exit,
}

/// Runs the attached line interface until `/exit` or the end of input.
///
/// # Errors
///
/// The editor cannot start; a lost connection.
pub(super) async fn run(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    let lines = Lines::start(start)?;
    let no_color = start.ui.no_color
        || start
            .launch
            .inherited("NO_COLOR")
            .is_some_and(|v| !v.is_empty());
    let screen = Screen {
        printer: lines.editor.printer(),
        display: Display::new(start.scope, no_color, start.ui.quiet),
        markdown: start.scope.config().display.final_response_markdown,
        scope: start.scope.clone(),
    };
    let mut chat = Chat {
        confirm_destructive: start.scope.config().cli.confirm_destructive,
        attached,
        start,
        lines,
        screen,
        signals: Signals::listen(),
        images: start.images.clone(),
        status_bar: crate::chat::status_bar::StatusBar::new(start.scope, start.launch),
        model: None,
        daemon_turn: false,
        prune: false,
        voice: crate::chat::voice::VoiceClient::new(start.scope, start.launch, start.cwd, "cli"),
        mouth: None,
    };
    chat.refresh_model().await;
    chat.screen.say(
        Role::Accent,
        &format!(
            "attached to the gateway daemon (pid {}) · session {} · /help for commands",
            chat.attached.daemon_pid, chat.attached.session
        ),
    );
    // A resumed session's summary (a new one has none), then the low-disk
    // prune question when it is due.
    chat.memory_summary().await;
    if let Some(question) = crate::daemon_client::autoprune_offer(&chat.attached.client).await {
        chat.screen.notice(question.as_str());
        chat.prune = true;
    }
    if let Some(first) = &start.first {
        chat.submit(first.clone()).await?;
    }
    let status = chat.main_loop().await;
    chat.memory_summary().await;
    status
}

impl Chat<'_, '_> {
    async fn main_loop(&mut self) -> Result<u8, Error> {
        let mut last_interrupt: Option<Instant> = None;
        loop {
            self.show_status();
            let typed = if let Some(line) = self.lines.queue.pop_front() {
                Typed::Line(line)
            } else {
                tokio::select! {
                    typed = self.lines.next() => typed,
                    _ = self.signals.next() => Typed::Interrupt,
                    Some(event) = self.attached.inbox.events.recv() => {
                        self.on_idle_event(&event);
                        continue;
                    }
                }
            };
            let line = match typed {
                Typed::Line(line) => line,
                Typed::Compose(buffer) => format!("/prompt {buffer}"),
                Typed::Eof => return Ok(0),
                Typed::Record if self.voice.mode.on => {
                    if let Flow::Exit = self.talk().await? {
                        return Ok(0);
                    }
                    continue;
                }
                Typed::Record => continue,
                Typed::Interrupt if self.daemon_turn => {
                    // A wakeup or continuation runs: stop it (a loop
                    // pauses), never arm the exit.
                    turn::interrupt(self.attached).await;
                    continue;
                }
                Typed::Interrupt => {
                    if last_interrupt.is_some_and(|t| t.elapsed() < DOUBLE_CTRL_C) {
                        return Ok(0);
                    }
                    last_interrupt = Some(Instant::now());
                    self.screen.notice("(press Ctrl+C again or Ctrl+D to exit)");
                    continue;
                }
            };
            last_interrupt = None;
            if let Flow::Exit = self.line(line).await? {
                return Ok(0);
            }
        }
    }

    pub(super) async fn line(&mut self, line: String) -> Result<Flow, Error> {
        if std::mem::take(&mut self.prune)
            && let Some(answer) = crate::chat::autoprune::Answer::parse(&line)
        {
            let word = match answer {
                crate::chat::autoprune::Answer::On => "on",
                crate::chat::autoprune::Answer::Off => "off",
                crate::chat::autoprune::Answer::Later => "later",
            };
            self.dispatch("autoprune", word).await?;
            return Ok(Flow::Continue);
        }
        let text = line.trim();
        if text.is_empty() {
            return Ok(Flow::Continue);
        }
        if text.starts_with('/') {
            return self.slash(text.to_owned()).await;
        }
        if text.starts_with('!') {
            self.screen.notice(
                "the shell escape runs in the session's terminal, which the gateway daemon holds; it is not available while attached",
            );
            return Ok(Flow::Continue);
        }
        self.submit(line).await?;
        Ok(Flow::Continue)
    }

    /// Sends `text` (with the pending images) as a turn.
    pub(super) async fn submit(&mut self, text: String) -> Result<(), Error> {
        let images = std::mem::take(&mut self.images)
            .into_iter()
            .map(|path| pmt_rpc::contracts::PromptImage::Path(path.to_string_lossy().into_owned()))
            .collect();
        self.run_turn(Prompt { text, images }).await
    }

    pub(super) async fn run_turn(&mut self, prompt: Prompt) -> Result<(), Error> {
        let mut asker = Asker::Line(&mut self.lines);
        // A voice turn with spoken replies speaks the answer as it streams.
        let speaking = self.mouth.as_deref().map(|mouth| super::voice::Speaking {
            screen: &self.screen,
            mouth,
        });
        let out: &dyn render::Out = match &speaking {
            Some(speaking) => speaking,
            None => &self.screen,
        };
        let outcome = turn::run(self.attached, prompt, &mut asker, out, &mut self.signals).await;
        match outcome {
            Ok(outcome) => {
                if let Some(finished) = &outcome.finished {
                    render::finished(&self.screen, finished);
                    if finished.exit_reason == "interrupted_by_user" {
                        self.screen.notice("(interrupted)");
                    }
                }
                Ok(())
            }
            // A refused submission (a busy or over-budget session) leaves
            // the interface running; a lost connection ends it.
            Err(err) if err.code().code == pmt_core::error::codes::RPC_CONNECTION.code => Err(err),
            Err(err) => {
                self.screen.error(&err);
                Ok(())
            }
        }
    }

    /// The session's model, for the status line.
    pub(super) async fn refresh_model(&mut self) {
        let params = SessionRef {
            session_id: self.attached.session.clone(),
        };
        let options = self
            .attached
            .client
            .call_typed::<pmt_rpc::contracts::ModelOptionsList>(&params)
            .await;
        self.model = options.ok().and_then(|o| o.current);
    }

    /// An event between this client's own turns: notices posted between
    /// turns (`/refine`), the turns the daemon runs on its own and the
    /// automation rows.
    fn on_idle_event(&mut self, event: &Envelope) {
        if event
            .session_id
            .as_deref()
            .is_some_and(|id| id != self.attached.session)
        {
            return;
        }
        match event.event.as_str() {
            "automation.state" => {
                let session = self.attached.session.clone();
                render::automation(&mut self.attached.automation, &session, &event.data);
            }
            "turn.finished" => {
                self.daemon_turn = false;
                if event.data["exit_reason"] == "interrupted_by_user" {
                    self.screen.notice("(interrupted)");
                }
            }
            "session.event" if !render::is_notice(&event.data) => self.daemon_turn = true,
            _ => {}
        }
        show_followed(event, &self.screen);
    }

    /// Hands the bottom status line to the editor (none when quiet or
    /// turned off), with the session's live automation rows (`loop ·
    /// every 5m · 2 runs · next in 4m`).
    fn show_status(&self) {
        let text = (!self.start.ui.quiet)
            .then(|| {
                crate::chat::status_bar::line_text(
                    &self.status_bar,
                    self.start.scope.name().as_str(),
                    self.model.as_deref(),
                )
            })
            .flatten()
            .map(|line| {
                let rows: Vec<&str> = self
                    .attached
                    .automation
                    .values()
                    .map(String::as_str)
                    .collect();
                if rows.is_empty() {
                    line
                } else {
                    format!("{line} │ {}", rows.join(" │ "))
                }
            });
        self.lines.set_status(text);
    }

    /// Runs `name` on the daemon's session (`command.dispatch`).
    pub(super) async fn dispatch(&mut self, name: &str, args: &str) -> Result<(), Error> {
        let params = DispatchParams {
            session_id: self.attached.session.clone(),
            name: name.to_owned(),
            args: args.to_owned(),
        };
        let answer = self
            .attached
            .client
            .call_typed::<CommandDispatch>(&params)
            .await
            .map_err(|e| error_of(&e));
        match answer {
            Ok(Dispatched::Done { notice }) => self.screen.notice(&notice),
            Ok(Dispatched::Refused { reason }) => self.screen.notice(&reason),
            Ok(Dispatched::Send { message, images }) => {
                self.run_turn(Prompt {
                    text: message,
                    images,
                })
                .await?;
            }
            Err(err) if err.code().code == pmt_core::error::codes::RPC_CONNECTION.code => {
                return Err(err);
            }
            Err(err) => self.screen.error(&err),
        }
        if name == "model" {
            self.refresh_model().await;
        }
        Ok(())
    }

    /// The current session's memory summary, when it has one (it ends or
    /// was resumed).
    async fn memory_summary(&self) {
        let summary =
            crate::daemon_client::memory_summary(&self.attached.client, &self.attached.session)
                .await;
        if let Some(line) = summary {
            self.screen.notice(&line);
        }
    }

    /// Leaves the current session for `next` (or reports why not).
    pub(super) async fn switch(&mut self, next: Result<String, Error>) {
        match next {
            Ok(next) => {
                self.memory_summary().await;
                let old = SessionRef {
                    session_id: std::mem::replace(&mut self.attached.session, next),
                };
                self.attached.automation.clear();
                self.daemon_turn = false;
                let _ = self
                    .attached
                    .client
                    .call_typed::<pmt_rpc::contracts::SessionDetach>(&old)
                    .await;
                self.screen
                    .notice(&format!("session {}", self.attached.session));
                self.memory_summary().await;
            }
            Err(err) => self.screen.error(&err),
        }
    }
}

/// Follows a session another client drives, read-only: its turns are
/// shown as they run until Ctrl-C or the end of input.
///
/// # Errors
///
/// `session.attach` errors.
pub(super) async fn follow(attached: &mut Attached, start: &Start<'_>) -> Result<u8, Error> {
    let params = pmt_rpc::contracts::AttachParams {
        role: pmt_rpc::contracts::AttachRole::Observer,
        ..pmt_rpc::contracts::AttachParams::driver(attached.session.clone())
    };
    attached
        .client
        .call_typed::<pmt_rpc::contracts::SessionAttach>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    let out = render::Plain {
        quiet: start.ui.quiet,
        json: false,
    };
    out.notice(&format!(
        "following session {} read-only; Ctrl+C leaves",
        attached.session
    ));
    let mut signals = Signals::listen();
    loop {
        tokio::select! {
            event = attached.inbox.events.recv() => {
                let Some(event) = event else { return Ok(0) };
                if event.event == "automation.state"
                    && let Some(row) = event.data["row"].as_str()
                {
                    out.notice(row);
                }
                show_followed(&event, &out);
            }
            _ = signals.next() => return Ok(0),
        }
    }
}

fn show_followed(event: &pmt_rpc::contracts::Envelope, out: &dyn Out) {
    match event.event.as_str() {
        "session.event" if event.data["type"] == "user.message" => {
            let text = event.data["payload"]["text"].as_str().unwrap_or_default();
            out.notice(&format!("› {text}"));
        }
        "session.event" => render::event(out, &event.data),
        "turn.finished" => {
            if let Ok(finished) = serde_json::from_value(event.data.clone()) {
                render::finished(out, &finished);
            }
        }
        _ => {}
    }
}
