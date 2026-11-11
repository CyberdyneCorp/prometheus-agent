//! The line interface (spec: cli, Interactive input handling; Busy policy
//! enforcement; Destructive command confirmation; Skill invocation as
//! user message; Quick commands; Themes are data; Usage display;
//! Conversation recap on resume; Seed prompt and prompt file).
//!
//! One loop owns the session: it asks [`input::Input`] for lines, runs
//! idle lines (a slash command, a skill invocation or a user turn) and,
//! while a turn runs ([`turn`]), keeps reading so typed commands follow
//! their busy policy and prompts (approvals, clarify questions, memory
//! approvals, confirmations) are answered from the next line.

mod automation;
mod bottom_line;
mod commands;
pub(crate) mod compose;
pub mod input;
mod layout;
mod progress;
mod prompts;
pub mod queue;
mod quick;
mod sessions;
pub mod shell;
mod turn;
mod voice;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_agent::tools::clarify::{ClarifyAnswer, ClarifyRequest};
use pmt_core::error::Error;
use pmt_core::human::Surface as Prompts;
use pmt_store::EndReason;
use pmt_tools::approval::{ApprovalRequest, Decision};
use tokio::sync::mpsc;

use self::input::{Event, Input};
use self::prompts::Pending;
pub use self::prompts::{CliSecretPrompt, MemoryApprover, MemoryAsk, SecretAsk};
use super::Ui;
use super::display::Display;
use super::session::{Factory, Live};
use super::theme::Role;
use crate::chat::session::browser::InstallAsk;
use crate::slash::{self, Registry};

/// A second Ctrl+C within this long exits an idle REPL.
pub const DOUBLE_CTRL_C: Duration = Duration::from_secs(2);

/// How the interactive session starts.
#[derive(Debug, Clone, Default)]
pub struct Start {
    /// The session was resumed (print the recap).
    pub resumed: bool,
    /// `--query` / `--prompt-file` text: the first user turn, verbatim.
    pub seed: Option<String>,
    /// `--run-budget`.
    pub run_budget: Option<Duration>,
    /// This terminal's identity (`-c` per terminal).
    pub terminal: Option<String>,
    /// `--image` attachments: they go with the first prompt.
    pub images: Vec<crate::chat::attach::Image>,
}

/// What the REPL does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

/// The REPL.
struct Repl {
    factory: Factory,
    live: Option<Live>,
    input: Input,
    registry: Registry,
    display: Display,
    queue: VecDeque<String>,
    pending: Option<Pending>,
    approvals: Option<Prompts<ApprovalRequest, Decision>>,
    clarify: Option<Prompts<ClarifyRequest, ClarifyAnswer>>,
    /// The session's tool activity and what this turn showed of it.
    progress: Option<mpsc::UnboundedReceiver<crate::chat::session::progress::Progress>>,
    view: progress::TurnView,
    memory: mpsc::UnboundedReceiver<MemoryAsk>,
    /// Runtime install questions (`Install <name> <version> (<size>)?`).
    installs: mpsc::UnboundedReceiver<InstallAsk>,
    /// Skill secrets to read hidden.
    secrets: mpsc::UnboundedReceiver<SecretAsk>,
    /// Messages background work posts to the chat (`/refine` summaries).
    posted: mpsc::UnboundedSender<String>,
    to_post: mpsc::UnboundedReceiver<String>,
    /// Background delegation completions waiting for the idle prompt.
    completions: mpsc::UnboundedReceiver<String>,
    last_interrupt: Option<Instant>,
    start: Start,
    confirm_destructive: bool,
    /// Images that go with the next prompt (`/image`, `--image`).
    attachments: Vec<crate::chat::attach::Image>,
    /// `!!` blocks that go with the next prompt.
    shell_context: Vec<String>,
    /// A turn runs (`/status`).
    running: bool,
    /// The bottom status line's toggles (`/statusbar`, `/battery`).
    status_bar: crate::chat::status_bar::StatusBar,
    /// The automated turn the next idle boundary runs (a goal
    /// continuation or a reserved timer wakeup).
    wakeup: Option<crate::automation::boundary::Reserved>,
    /// `/voice` and the voice conversations of this terminal.
    voice: crate::chat::voice::VoiceClient,
    /// Speaks the reply of the turn being run (a voice turn with spoken
    /// replies on).
    mouth: Option<Arc<pmt_voice::speaker::Mouth>>,
}

/// The memory approver of an interactive session and the REPL's end of
/// it (install it on the factory before the session opens).
#[must_use]
pub fn memory_channel() -> (Arc<MemoryApprover>, mpsc::UnboundedReceiver<MemoryAsk>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(MemoryApprover::new(tx)), rx)
}

/// The REPL's ends of the session's yes/no questions.
pub struct Asks {
    /// Memory changes needing approval.
    pub memory: mpsc::UnboundedReceiver<MemoryAsk>,
    /// Runtime installs.
    pub installs: mpsc::UnboundedReceiver<InstallAsk>,
    /// Skill secrets.
    pub secrets: mpsc::UnboundedReceiver<SecretAsk>,
    /// The secret prompt to start listening when the REPL runs.
    pub secret_prompt: Option<Arc<CliSecretPrompt>>,
    /// Background process notices, printed as their own messages.
    pub notices: mpsc::UnboundedReceiver<pmt_tools::terminal::process::Notice>,
    /// Background delegation completions: each runs as the agent's next
    /// input once the chat is idle.
    pub completions: mpsc::UnboundedReceiver<String>,
}

impl Asks {
    /// Receivers nothing sends to.
    #[must_use]
    pub fn none() -> Self {
        Self {
            memory: memory_channel().1,
            installs: mpsc::unbounded_channel().1,
            secrets: mpsc::unbounded_channel().1,
            secret_prompt: None,
            notices: mpsc::unbounded_channel().1,
            completions: mpsc::unbounded_channel().1,
        }
    }
}

/// Sleeps until `at`; never wakes without one.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Prints every background process notice above the prompt, as its own
/// message. The task holds a printer, so it must end (aborted and awaited)
/// before the input is dropped: the input waits for every printer.
fn print_notices(
    mut notices: mpsc::UnboundedReceiver<pmt_tools::terminal::process::Notice>,
    printer: input::Printer,
) -> tokio::task::JoinHandle<()> {
    pmt_core::spawn::spawn(async move {
        while let Some(notice) = notices.recv().await {
            printer.print(&format!("[background] {}", notice.text()));
        }
    })
}

/// Runs the line interface over `live` until the user leaves; returns the
/// exit status.
///
/// # Errors
///
/// The editor cannot start.
///
/// # Panics
///
/// Never: the `default` theme is built in.
pub async fn run(
    factory: Factory,
    live: Live,
    asks: Asks,
    start: Start,
    ui: Ui,
) -> Result<u8, Error> {
    let config = factory.scope.config().clone();
    let no_color_env = factory
        .launch()
        .inherited("NO_COLOR")
        .is_some_and(|v| !v.is_empty());
    let display = Display::new(&factory.scope, ui.no_color || no_color_env, ui.quiet);
    let status_bar = crate::chat::status_bar::StatusBar::new(&factory.scope, factory.launch());
    let input = Input::start(input::Setup {
        history: factory.scope.dir().join("cli_history"),
        cwd: factory.options.cwd.clone(),
        redactor: factory.redactor().clone(),
        vim_mode: config.display.vim_mode,
        record_key: input::editor_key(&config.voice.record_key.0),
    })?;
    let (posted, to_post) = mpsc::unbounded_channel();
    let factory_scope = factory.scope.clone();
    let factory_cwd = factory.options.cwd.clone();
    let launch = factory.launch().clone();
    let mut repl = Repl {
        factory,
        live: None,
        input,
        registry: Registry::new()?,
        display,
        queue: VecDeque::new(),
        pending: None,
        approvals: None,
        clarify: None,
        progress: None,
        view: progress::TurnView::default(),
        memory: asks.memory,
        installs: asks.installs,
        secrets: asks.secrets,
        completions: asks.completions,
        posted,
        to_post,
        last_interrupt: None,
        attachments: start.images.clone(),
        start,
        confirm_destructive: config.cli.confirm_destructive,
        shell_context: Vec::new(),
        running: false,
        status_bar,
        wakeup: None,
        voice: crate::chat::voice::VoiceClient::new(&factory_scope, &launch, &factory_cwd, "cli"),
        mouth: None,
    };
    repl.attach(live);
    let notices = print_notices(asks.notices, repl.input.printer());
    if let Some(prompt) = &asks.secret_prompt {
        prompt.listen();
    }
    repl.load_registry();
    repl.banner();
    if repl.start.resumed {
        repl.recap();
        repl.memory_summary(repl.live().id);
        Box::pin(repl.offer_resume()).await;
    }
    repl.offer_autoprune();
    repl.spacer();
    let status = Box::pin(repl.main_loop()).await;
    notices.abort();
    let _ = notices.await;
    Ok(status)
}

impl Repl {
    async fn main_loop(&mut self) -> u8 {
        if let Some(seed) = self.start.seed.take() {
            // A seed is the first user turn verbatim: never a slash
            // command, shell escape or reference.
            self.user_turn(seed).await;
        }
        loop {
            // A queued line goes before any automated turn.
            let flow = match self.queue.pop_front() {
                Some(line) => {
                    self.drop_wakeup();
                    Box::pin(self.idle_line(line)).await
                }
                None => match self.wakeup.take() {
                    Some(reserved) => {
                        self.automated(reserved).await;
                        Flow::Continue
                    }
                    None => Box::pin(self.idle_event()).await,
                },
            };
            if flow == Flow::Exit {
                break;
            }
        }
        self.finish(EndReason::UserExit).await;
        0
    }

    /// Reads and handles one event at the idle prompt.
    async fn idle_event(&mut self) -> Flow {
        let prompt = self.prompt();
        self.show_status();
        self.input.request(&prompt);
        let automation = self.automation_deadline();
        let event = tokio::select! {
            event = self.input.next() => event,
            () = sleep_until(automation) => {
                self.automation_due();
                return Flow::Continue;
            }
            Some(text) = self.to_post.recv() => {
                self.notice(&text);
                return Flow::Continue;
            }
            Some(completion) = self.completions.recv() => {
                self.notice("Background subagents finished; their results go to the agent.");
                self.queue.push_back(completion);
                return Flow::Continue;
            }
        };
        match event {
            Event::Line(line) => {
                self.last_interrupt = None;
                Box::pin(self.idle_line(line)).await
            }
            Event::Interrupt => self.idle_interrupt(),
            Event::Eof => Flow::Exit,
            Event::Compose(buffer) => {
                self.last_interrupt = None;
                self.compose(&buffer).await
            }
            Event::Record if self.voice.mode.on => {
                self.last_interrupt = None;
                Box::pin(self.talk()).await
            }
            Event::SuspendRefused | Event::Record => Flow::Continue,
        }
    }

    /// Ctrl+C at an idle prompt: a hint, or exit when it is the second
    /// within [`DOUBLE_CTRL_C`].
    fn idle_interrupt(&mut self) -> Flow {
        if let Some(pending) = self.pending.take() {
            self.input.set_masked(false);
            pending.cancel();
            self.notice("cancelled");
            return Flow::Continue;
        }
        let now = Instant::now();
        if self
            .last_interrupt
            .is_some_and(|at| now.duration_since(at) <= DOUBLE_CTRL_C)
        {
            return Flow::Exit;
        }
        self.last_interrupt = Some(now);
        self.notice("(press Ctrl+C again or Ctrl+D to exit)");
        Flow::Continue
    }

    /// One line typed while no turn runs.
    async fn idle_line(&mut self, line: String) -> Flow {
        if let Some(pending) = self.pending.take() {
            return Box::pin(self.answer(pending, &line)).await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Flow::Continue;
        }
        // A `yes` saves the job a `/cron` echo just showed; any other input
        // drops it.
        if let Some(confirmation) = crate::chat::commands::cron::take(self.live().id, trimmed) {
            let session = self.live();
            self.show(crate::chat::commands::cron::confirm(
                &self.ctx(session),
                confirmation,
            ));
            return Flow::Continue;
        }
        if trimmed.starts_with('/') {
            return Box::pin(self.slash(trimmed.to_owned())).await;
        }
        if trimmed.starts_with('!') {
            self.shell_escape(trimmed).await;
            return Flow::Continue;
        }
        self.user_turn(line).await;
        Flow::Continue
    }

    /// Hands the bottom status line to the editor (drawn before the read;
    /// none when quiet or turned off).
    fn show_status(&self) {
        let text = (!self.display.quiet).then(|| {
            let target = self.live().model.target();
            let model = format!(
                "{}/{}",
                target.provider,
                target.model.as_deref().unwrap_or("default")
            );
            let line = crate::chat::status_bar::line_text(
                &self.status_bar,
                self.factory.scope.name().as_str(),
                Some(&model),
            )?;
            // The live automation rows (`loop · every 5m · 2 runs · next
            // in 4m`).
            let rows = self.automation_rows();
            Some(if rows.is_empty() {
                line
            } else {
                format!("{line} │ {}", rows.join(" │ "))
            })
        });
        self.input.set_status(text.flatten());
    }

    fn live(&self) -> &Live {
        self.live.as_ref().expect("a live session")
    }

    /// Makes `live` the current session.
    fn attach(&mut self, mut live: Live) {
        self.approvals = live.approvals.take();
        self.clarify = live.clarify.take();
        self.progress = live.progress.take();
        // Memory notices come between turns too (a background review):
        // the idle loop shows what is posted.
        live.memory
            .setup()
            .notices
            .bind(Arc::new(Posted(self.posted.clone())));
        for notice in std::mem::take(&mut live.notices) {
            self.notice(&notice);
        }
        if let Some(terminal) = &self.start.terminal
            && let Err(err) = self.factory.store.record_terminal(live.id, terminal)
        {
            tracing::warn!(error = %err, "cannot record the terminal");
        }
        self.live = Some(live);
    }

    /// Ends the current session.
    async fn finish(&mut self, reason: EndReason) {
        if let Some(live) = self.live.take() {
            self.memory_summary(live.id);
            live.close(&self.factory.store, reason).await;
        }
    }

    /// `Memory: +a ~p -r (review with /memory review)` when the agent
    /// changed memory in session `id` (spec: memory, Reviewable memory
    /// changes per session): at the end and on resume.
    pub(super) fn memory_summary(&self, id: pmt_core::id::SessionId) {
        let mode = self.factory.scope.config().memory.review_mode;
        match pmt_memory::journal::summary(&self.factory.store, mode, &id.to_string()) {
            Ok(Some(line)) => self.notice(&line),
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "memory review summary skipped"),
        }
    }

    /// The commands of this session: built-ins, quick commands and skills.
    fn load_registry(&mut self) {
        self.registry = crate::chat::commands::registry(&self.factory.scope, self.live());
    }

    /// What the shared session commands run against.
    fn ctx<'a>(&'a self, live: &'a Live) -> crate::chat::commands::Ctx<'a> {
        let posted = self.posted.clone();
        crate::chat::commands::Ctx {
            factory: &self.factory,
            live,
            scope: &self.factory.scope,
            registry: &self.registry,
            surface: slash::Surface::Cli,
            timestamps: self.display.timestamps,
            running: self.running,
            post: Arc::new(move |text| {
                let _ = posted.send(text);
            }),
            message_limit: None,
        }
    }

    /// The profile's UI strings now (spec: i18n): a language or overlay
    /// change shows at the next prompt, and never reaches the model.
    fn strings(&self) -> std::sync::Arc<pmt_i18n::Strings> {
        pmt_config::language::strings(self.factory.scope.dir(), pmt_i18n::Surface::Core)
    }

    fn prompt(&self) -> String {
        let symbol = match &self.pending {
            Some(pending) => pending.prompt(&self.strings()),
            None if self.display.focus.is_some() => "focus › ".to_owned(),
            None => "› ".to_owned(),
        };
        self.display
            .theme
            .paint(Role::Prompt, &symbol, self.display.color)
    }

    fn banner(&self) {
        if self.display.quiet {
            return;
        }
        let target = self.live().model.target();
        let model = format!(
            "{}/{}",
            target.provider,
            target.model.as_deref().unwrap_or("default")
        );
        let banner = if self.factory.scope.config().display.compact {
            format!("Prometheus {} · {model}", crate::build_info::VERSION)
        } else {
            format!(
                "Prometheus {} · profile {} · {model} · /help for commands",
                crate::build_info::VERSION,
                self.factory.scope.name()
            )
        };
        self.say(Role::Accent, &banner);
        // Spec: security-boundaries, Hardening downgrades are never silent.
        if let Some(marker) = self.live().downgrades.banner_marker() {
            self.say(Role::Error, &format!("degraded: {marker}"));
        }
        self.tip(
            "slash-help",
            "Tip: type /help for the commands, /status for this session",
        );
    }

    /// Shows the one-time tip `id` unless this profile has seen it (spec:
    /// cli, Onboarding state).
    fn tip(&self, id: &str, text: &str) {
        match self
            .factory
            .store
            .mark_onboarding(pmt_store::onboarding::Step::Tip, id)
        {
            Ok(true) => self.notice(text),
            Ok(false) => {}
            Err(err) => tracing::warn!(error = %err, "cannot record the onboarding tip"),
        }
    }

    fn recap(&self) {
        let live = self.live();
        let store = &self.factory.store;
        let (Ok(session), Ok(messages)) = (
            store.session(live.id),
            store.messages(live.id, pmt_store::Visibility::Active),
        ) else {
            return;
        };
        if let Some(text) =
            super::recap::render(&session, &messages, &self.factory.scope.config().display)
        {
            self.say(Role::Notice, &text);
        }
    }

    /// Prints `text` in `role`'s colour.
    fn say(&self, role: Role, text: &str) {
        self.input
            .print(&self.display.theme.paint(role, text, self.display.color));
    }

    fn notice(&self, text: &str) {
        self.say(Role::Notice, text);
    }

    fn error(&self, err: &Error) {
        let text = crate::output::render_error(err, false);
        self.say(Role::Error, text.trim_end());
    }
}

/// Memory notices of the line interface's session, shown by its loop.
struct Posted(mpsc::UnboundedSender<String>);

impl pmt_memory::notices::NoticeSink for Posted {
    fn show(&self, text: String) {
        let _ = self.0.send(text);
    }
}
