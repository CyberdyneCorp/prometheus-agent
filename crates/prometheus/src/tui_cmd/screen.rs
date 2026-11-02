//! The TUI's event loop: terminal keys, the session's events and server
//! requests, results of requests running in the background, signals and
//! the busy animation, each turned into a change of the screen. The
//! terminal is entered only once the session is open (so connection
//! errors print normally) and restored on every way out.

use std::collections::{HashSet, VecDeque};
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::Event as TermEvent;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::secret::Redactor;
use pmt_rpc::client::ServerCall;
use pmt_rpc::contracts::{
    Completions, ContextUsage, Dispatched, ModelOptions, PromptImage, Submitted,
};
use pmt_rpc::discovery::DaemonRecord;
use pmt_tui::app::App;
use pmt_tui::history::{self, History};
use pmt_tui::keys::Bindings;
use pmt_tui::status::Status;
use pmt_tui::terminal::TerminalGuard;
use pmt_tui::theme::Theme;
use pmt_tui::transcript::Kind;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use super::link::Link;
use super::signals::Signals;
use super::{Ending, Launch};

/// What the TUI starts with.
pub(super) struct Open<'a> {
    /// What to open.
    pub(super) launch: &'a Launch,
    /// The profile's configuration.
    pub(super) scope: &'a ProfileScope,
    /// The key bindings.
    pub(super) bindings: Bindings,
    /// `--no-color` or `NO_COLOR`.
    pub(super) no_color: bool,
    /// The home root (logs, discovery).
    pub(super) home: PathBuf,
    /// Debug builds: panic while drawing (`PMT_TEST_TUI_PANIC`).
    pub(super) panic_on_draw: bool,
}

/// The result of a request that ran in the background.
pub(super) enum Done {
    /// `command.dispatch` answered.
    Dispatched(Result<Dispatched, Error>),
    /// `prompt.submit` answered.
    Submitted(Result<Submitted, Error>),
    /// `session.context_usage` answered.
    Usage(ContextUsage),
    /// `model.options` answered.
    Models(ModelOptions),
    /// A completion request answered for the word starting at the offset.
    Completed(usize, Completions),
    /// A session operation (`/new`, `/resume`, `/fork`) opened this one.
    Switched(Result<(String, pmt_rpc::contracts::Attached), Error>),
}

/// The TUI's state.
pub(super) struct Tui<'a> {
    pub(super) app: App,
    pub(super) link: Link,
    pub(super) open: &'a Open<'a>,
    pub(super) history: History,
    /// Server requests waiting for an answer; the first is on screen.
    pub(super) asking: VecDeque<Asked>,
    /// The answers given so far to the shown clarify request's questions.
    pub(super) clarify: Vec<serde_json::Value>,
    /// What was typed into a secret or sudo card: kept apart from the
    /// composer so it is never drawn, never in the history.
    pub(super) masked: String,
    /// Turns this TUI admitted that have not finished.
    pub(super) pending: HashSet<String>,
    /// Where background requests report.
    pub(super) done: mpsc::UnboundedSender<Done>,
    /// Set when the TUI should end.
    pub(super) ending: Option<Ending>,
    /// Debug builds: panic on the next draw (`PMT_TEST_TUI_PANIC`).
    panic_on_draw: bool,
    /// The connection is lost: give up re-attaching after this.
    lost: Option<Lost>,
}

/// A server request on screen or waiting for it.
pub(super) struct Asked {
    pub(super) call: ServerCall,
    /// When it settles unanswered (its `deadline_ms` from arrival).
    pub(super) deadline: Option<tokio::time::Instant>,
}

impl Asked {
    pub(super) fn new(call: ServerCall) -> Self {
        let deadline = call.params["deadline_ms"]
            .as_u64()
            .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
        Self { call, deadline }
    }

    pub(super) fn method(&self) -> &str {
        &self.call.method
    }
}

/// A lost connection being re-established.
struct Lost {
    deadline: tokio::time::Instant,
    close_code: Option<u16>,
}

type Screen = Terminal<CrosstermBackend<Stdout>>;

/// Opens the session, then runs the screen until it ends.
///
/// # Errors
///
/// Connection, session and terminal errors.
pub(super) async fn run(open: &Open<'_>, record: DaemonRecord) -> Result<Ending, Error> {
    let mut signals = Signals::listen();
    let profile = open.scope.name().as_str().to_owned();
    let (link, notice) = Link::open(record, &profile, open.launch).await?;
    pmt_tui::terminal::install_panic_hook(open.home.join("logs"));
    let guard = TerminalGuard::enter().map_err(|e| terminal_error(&e))?;
    let mut screen =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(|e| terminal_error(&e))?;
    let (done, mut results) = mpsc::unbounded_channel();
    let mut tui = Tui::new(open, link, done);
    if let Some(notice) = notice {
        tui.app.transcript.push(Kind::Notice, notice);
    }
    let support = &tui.link.client.hello().platform_support;
    if support.tier == "unsupported" {
        let note = format!(
            "the gateway runs on {}, an unsupported target",
            support.target
        );
        tui.app.transcript.push(Kind::Notice, note);
    }
    tui.reload().await;
    if let Some(first) = &open.launch.first {
        tui.submit(first.clone(), open.launch.images.clone());
    }
    let keys = KeyReader::start();
    let ending = tui
        .run(&mut screen, &keys, &mut results, &mut signals)
        .await;
    // Restore first: stopping the reader is bounded but may give up on it.
    drop(guard);
    keys.stop();
    ending
}

fn terminal_error(e: &std::io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("cannot use the terminal: {e}"))
}

impl<'a> Tui<'a> {
    fn new(open: &'a Open<'a>, link: Link, done: mpsc::UnboundedSender<Done>) -> Self {
        let config = open.scope.config();
        // The profile the daemon granted (the handshake names it).
        let granted = link.client.hello().profiles.first().cloned();
        let status = Status {
            profile: granted.unwrap_or_else(|| open.scope.name().as_str().to_owned()),
            reduced_motion: config.ui.tui.reduced_motion,
            ..Status::default()
        };
        let theme = Theme::new(config.ui.tui.theme, open.no_color);
        let history = History::load(history::path_in(open.scope.dir()), Redactor::default());
        Self {
            app: App::new(theme, status),
            link,
            open,
            history,
            asking: VecDeque::new(),
            clarify: Vec::new(),
            masked: String::new(),
            pending: HashSet::new(),
            done,
            ending: None,
            panic_on_draw: open.panic_on_draw,
            lost: None,
        }
    }

    async fn run(
        &mut self,
        screen: &mut Screen,
        keys: &KeyReader,
        results: &mut mpsc::UnboundedReceiver<Done>,
        signals: &mut Signals,
    ) -> Result<Ending, Error> {
        let mut tick = tokio::time::interval(Duration::from_millis(120));
        loop {
            self.draw(screen)?;
            if let Some(ending) = self.ending.take() {
                return Ok(ending);
            }
            let mut terminal_events = keys.events.lock().await;
            tokio::select! {
                Some(event) = terminal_events.recv() => {
                    drop(terminal_events);
                    self.on_terminal(event);
                }
                event = self.link.inbox.events.recv(), if self.lost.is_none() => {
                    drop(terminal_events);
                    match event {
                        Some(event) => self.on_event(&event),
                        None => self.connection_lost().await,
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(250)), if self.lost.is_some() => {
                    drop(terminal_events);
                    self.reconnect().await?;
                }
                Some(call) = self.link.inbox.requests.recv() => {
                    drop(terminal_events);
                    self.on_request(call);
                }
                Some(result) = results.recv() => {
                    drop(terminal_events);
                    self.on_done(result).await;
                }
                code = signals.next() => return Ok(Ending::Signal(code)),
                _ = tick.tick(), if self.app.status.busy || self.counting_down() => {
                    self.app.status.frame = self.app.status.frame.wrapping_add(1);
                    // The card's countdown.
                    self.show_question();
                }
            }
        }
    }

    fn draw(&mut self, screen: &mut Screen) -> Result<(), Error> {
        let panic_now = self.panic_on_draw;
        screen
            .draw(|frame| {
                assert!(!panic_now, "PMT_TEST_TUI_PANIC: a panic while drawing");
                self.app.render(frame);
            })
            .map(drop)
            .map_err(|e| terminal_error(&e))
    }

    /// The card on screen has a deadline to count down.
    fn counting_down(&self) -> bool {
        self.asking.front().is_some_and(|a| a.deadline.is_some())
    }

    /// Shows a notice line.
    pub(super) fn notice(&mut self, text: impl Into<String>) {
        self.app.transcript.push(Kind::Notice, text);
    }

    /// Shows an error line.
    pub(super) fn error(&mut self, err: &Error) {
        let text = crate::output::render_error(err, false);
        self.app
            .transcript
            .push(Kind::Error, text.trim_end().to_owned());
    }

    /// Runs `request` in the background; its result comes back as a
    /// [`Done`].
    pub(super) fn background<F>(&self, request: F)
    where
        F: Future<Output = Done> + Send + 'static,
    {
        let done = self.done.clone();
        tokio::spawn(async move {
            let _ = done.send(request.await);
        });
    }

    /// Submits `text` (with `images`) as a prompt; the admission comes back
    /// as [`Done::Submitted`].
    pub(super) fn submit(&mut self, text: String, images: Vec<PromptImage>) {
        let params = pmt_rpc::contracts::SubmitParams {
            session_id: self.link.session.clone(),
            text,
            images,
            run_budget_secs: self.open.launch.run_budget,
            input_id: Some(ulid::Ulid::generate().to_string()),
            author: None,
        };
        let client = self.link.client.clone();
        self.background(async move {
            let submitted = client
                .call_typed::<pmt_rpc::contracts::PromptSubmit>(&params)
                .await
                .map_err(|e| pmt_rpc::client::error_of(&e));
            Done::Submitted(submitted)
        });
    }

    /// The connection ended: the screen says so while [`Tui::reconnect`]
    /// finds the daemon again.
    async fn connection_lost(&mut self) {
        let closed = self.link.client.wait_closed().await;
        self.app.status.note = Some("reconnecting".to_owned());
        self.asking.clear();
        self.clarify.clear();
        self.masked.clear();
        self.app.card = None;
        self.app.status.approvals = 0;
        self.lost = Some(Lost {
            deadline: tokio::time::Instant::now() + Duration::from_secs(30),
            close_code: closed.code,
        });
    }

    /// One attempt to find the daemon and re-attach from the watermark
    /// (spec: tui, Event sequencing and reconnect replay); the transcript
    /// is reloaded when the daemon could not replay what was missed.
    async fn reconnect(&mut self) -> Result<(), Error> {
        let Some(lost) = &self.lost else {
            return Ok(());
        };
        if let Some(record) = crate::daemon_client::running(&self.open.home)?
            && let Ok(replayed) = self.link.reconnect(record).await
        {
            self.lost = None;
            self.app.status.note = None;
            if !replayed {
                self.reload().await;
            }
            return Ok(());
        }
        if tokio::time::Instant::now() < lost.deadline {
            return Ok(());
        }
        let code = lost
            .close_code
            .map_or_else(|| "no close code".to_owned(), |c| c.to_string());
        Err(Error::new(
            &codes::RPC_CONNECTION,
            format!(
                "the connection to the gateway daemon was lost ({code}) and it did not come back"
            ),
        )
        .with_hint("see 'prometheus daemon status'; the session is kept by the daemon"))
    }
}

/// Reads terminal events on a thread of its own (crossterm's reads
/// block) and hands them to the loop; it stops within 50 ms of
/// [`KeyReader::stop`], before anything else reads the terminal.
///
/// Except when the terminal has hung up: crossterm 0.29's read loop then
/// spins on a readable terminal with nothing to read and never returns to
/// check its timeout, so [`KeyReader::stop`] waits a bounded time and
/// leaves such a thread to end with the process.
struct KeyReader {
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<TermEvent>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyReader {
    fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        let sent = crossterm::event::read().map(|event| tx.send(event));
                        if !matches!(sent, Ok(Ok(()))) {
                            return;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        Self {
            events: tokio::sync::Mutex::new(rx),
            stop,
            thread: Some(thread),
        }
    }

    /// How long [`KeyReader::stop`] waits for the thread: a few of its
    /// 50 ms polls.
    const STOP_WAIT: Duration = Duration::from_millis(200);

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + Self::STOP_WAIT;
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}
