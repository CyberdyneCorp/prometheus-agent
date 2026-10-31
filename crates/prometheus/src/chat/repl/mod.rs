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

mod commands;
mod compose;
pub mod info;
pub mod input;
mod progress;
mod prompts;
pub mod queue;
mod quick;
mod refine;
mod sessions;
pub mod shell;
mod turn;

use std::collections::VecDeque;
use std::io::IsTerminal as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_agent::tools::clarify::{ClarifyAnswer, ClarifyRequest};
use pmt_config::schema::ToolProgress;
use pmt_core::error::Error;
use pmt_core::human::Surface as Prompts;
use pmt_store::EndReason;
use pmt_tools::approval::{ApprovalRequest, Decision};
use tokio::sync::mpsc;

use self::input::{Event, Input};
use self::prompts::Pending;
pub use self::prompts::{MemoryApprover, MemoryAsk};
use super::Ui;
use super::session::{Factory, Live};
use super::theme::{Role, Theme};
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

/// Display state the user can change in the session.
struct Display {
    theme: Theme,
    color: bool,
    quiet: bool,
    progress: ToolProgress,
    /// `/focus on`: the mode `/focus off` restores.
    focus: Option<ToolProgress>,
    /// `display.timestamps` (`/timestamps`).
    timestamps: bool,
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
    /// Messages background work posts to the chat (`/refine` summaries).
    posted: mpsc::UnboundedSender<String>,
    to_post: mpsc::UnboundedReceiver<String>,
    last_interrupt: Option<Instant>,
    start: Start,
    confirm_destructive: bool,
    /// Images that go with the next prompt (`/image`, `--image`).
    attachments: Vec<crate::chat::attach::Image>,
    /// `!!` blocks that go with the next prompt.
    shell_context: Vec<String>,
    /// A turn runs (`/status`).
    running: bool,
}

/// The memory approver of an interactive session and the REPL's end of
/// it (install it on the factory before the session opens).
#[must_use]
pub fn memory_channel() -> (Arc<MemoryApprover>, mpsc::UnboundedReceiver<MemoryAsk>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(MemoryApprover::new(tx)), rx)
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
    memory: mpsc::UnboundedReceiver<MemoryAsk>,
    start: Start,
    ui: Ui,
) -> Result<u8, Error> {
    let config = factory.scope.config().clone();
    let no_color_env = factory
        .launch()
        .inherited("NO_COLOR")
        .is_some_and(|v| !v.is_empty());
    let color =
        super::theme::color_enabled(ui.no_color, no_color_env, std::io::stdout().is_terminal());
    let theme = Theme::load(factory.scope.dir(), &config.display.theme).unwrap_or_else(|why| {
        tracing::warn!(theme = %config.display.theme, %why, "theme unavailable; using default");
        Theme::load(factory.scope.dir(), "default").expect("built-in theme")
    });
    let input = Input::start(input::Setup {
        history: factory.scope.dir().join("cli_history"),
        cwd: factory.options.cwd.clone(),
        redactor: factory.redactor().clone(),
    })?;
    let (posted, to_post) = mpsc::unbounded_channel();
    let mut repl = Repl {
        factory,
        live: None,
        input,
        registry: Registry::new()?,
        display: Display {
            theme,
            color,
            quiet: ui.quiet,
            progress: config.display.tool_progress,
            focus: None,
            timestamps: config.display.timestamps,
        },
        queue: VecDeque::new(),
        pending: None,
        approvals: None,
        clarify: None,
        progress: None,
        view: progress::TurnView::default(),
        memory,
        posted,
        to_post,
        last_interrupt: None,
        attachments: start.images.clone(),
        start,
        confirm_destructive: config.cli.confirm_destructive,
        shell_context: Vec::new(),
        running: false,
    };
    repl.attach(live);
    repl.load_registry();
    repl.banner();
    if repl.start.resumed {
        repl.recap();
    }
    Ok(Box::pin(repl.main_loop()).await)
}

impl Repl {
    async fn main_loop(&mut self) -> u8 {
        if let Some(seed) = self.start.seed.take() {
            // A seed is the first user turn verbatim: never a slash
            // command, shell escape or reference.
            self.user_turn(seed).await;
        }
        loop {
            let flow = match self.queue.pop_front() {
                Some(line) => self.idle_line(line).await,
                None => self.idle_event().await,
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
        self.input.request(&prompt);
        let event = tokio::select! {
            event = self.input.next() => event,
            Some(text) = self.to_post.recv() => {
                self.notice(&text);
                return Flow::Continue;
            }
        };
        match event {
            Event::Line(line) => {
                self.last_interrupt = None;
                self.idle_line(line).await
            }
            Event::Interrupt => self.idle_interrupt(),
            Event::Eof => Flow::Exit,
            Event::Compose(buffer) => {
                self.last_interrupt = None;
                self.compose(&buffer).await
            }
            Event::SuspendRefused => Flow::Continue,
        }
    }

    /// Ctrl+C at an idle prompt: a hint, or exit when it is the second
    /// within [`DOUBLE_CTRL_C`].
    fn idle_interrupt(&mut self) -> Flow {
        if let Some(pending) = self.pending.take() {
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
            return self.answer(pending, &line).await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Flow::Continue;
        }
        if trimmed.starts_with('/') {
            return self.slash(trimmed.to_owned()).await;
        }
        if trimmed.starts_with('!') {
            self.shell_escape(trimmed).await;
            return Flow::Continue;
        }
        self.user_turn(line).await;
        Flow::Continue
    }

    fn live(&self) -> &Live {
        self.live.as_ref().expect("a live session")
    }

    /// Makes `live` the current session.
    fn attach(&mut self, mut live: Live) {
        self.approvals = live.approvals.take();
        self.clarify = live.clarify.take();
        self.progress = live.progress.take();
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
            live.close(&self.factory.store, reason).await;
        }
    }

    /// The commands of this session: built-ins, quick commands and skills.
    fn load_registry(&mut self) {
        let mut registry = match Registry::new() {
            Ok(registry) => registry,
            Err(err) => {
                self.error(&err);
                return;
            }
        };
        quick::register(&mut registry, &self.factory.scope.config().quick_commands);
        let core = registry.builtin_names();
        let table = pmt_skills::slash::table(&self.live().skills, &core);
        for (slug, skill) in &table.commands {
            let declaration = slash::Declaration {
                name: slug.clone(),
                description: format!("skill {skill}"),
                ..slash::Declaration::default()
            };
            if let Err(err) = registry.register(&declaration, slash::Source::Skill) {
                self.error(&err);
            }
        }
        for warning in &registry.warnings {
            tracing::warn!("{warning}");
        }
        self.registry = registry;
    }

    fn prompt(&self) -> String {
        let symbol = match &self.pending {
            Some(pending) => pending.prompt(),
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
        let banner = format!(
            "Prometheus {} · profile {} · {model} · /help for commands",
            crate::build_info::VERSION,
            self.factory.scope.name()
        );
        self.say(Role::Accent, &banner);
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
