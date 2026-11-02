//! The line interface's input (spec: cli, Interactive input handling):
//! one thread owns the line editor and reads a line whenever the REPL asks
//! for one, so the user can type while a turn runs; text printed while a
//! line is being edited appears above the prompt.
//!
//! A trailing `\` or Alt+Enter continues the line; history lives in
//! `<profile>/cli_history` (at most [`HISTORY_CAP`] entries, secrets
//! redacted, owner-only); `@` completes files under the session's working
//! directory. In the editor Ctrl+C is a key (no signal), reported as
//! [`Event::Interrupt`]; Ctrl+D on an empty line is [`Event::Eof`].
//! Ctrl+X Ctrl+E hands the buffer to the external editor
//! ([`Event::Compose`]). Ctrl+Z at an idle prompt ends the read with its
//! buffer kept, the input thread stops the process with SIGTSTP (terminal
//! mode already restored) and, on `fg`, reads again with the prompt and
//! the buffer redrawn; while a turn runs it is refused
//! ([`Event::SuspendRefused`]) (spec: cli, External editor composition;
//! Job-control suspend).
//!
//! Output goes through one printer thread, in order: the editor's external
//! printer hands a line to a running read through a one-slot channel and
//! blocks while the slot is full, and a read can end (the user pressed
//! Enter) with the slot still full. The REPL thread must never wait on
//! that, or it deadlocks with an editor that only drains the slot during
//! its next read; the printer thread waits instead. Each request for a
//! line first waits until the queue is written, so output printed between
//! two reads always lands before the next prompt.
//!
//! Why not the editor's own suspend: it sends SIGTSTP to the process group,
//! a process-directed signal that Linux may hand to another thread, so the
//! input thread can redraw the prompt before the process actually stops
//! and nothing is redrawn after `fg`. A signal raised on the input thread
//! itself stops the process before that thread runs on.

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::secret::Redactor;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::{FileHistory, History as _};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{
    Cmd, ColorMode, ConditionalEventHandler, Config, EditMode, Editor, EventContext, EventHandler,
    ExternalPrinter, Helper, KeyCode, KeyEvent, Modifiers, RepeatCount,
};
use tokio::sync::mpsc;

/// Most history entries kept.
pub const HISTORY_CAP: usize = 10_000;

/// What the user did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A submitted line (continuations joined with newlines).
    Line(String),
    /// Ctrl+C.
    Interrupt,
    /// Ctrl+D on an empty line, or the terminal went away.
    Eof,
    /// Ctrl+X Ctrl+E with the buffer so far.
    Compose(String),
    /// Ctrl+Z while a turn runs (the line is still being read).
    SuspendRefused,
}

/// State the key handlers share with the REPL.
#[derive(Default)]
struct Shared {
    /// A turn runs: Ctrl+Z is refused.
    busy: AtomicBool,
    /// The buffer Ctrl+X Ctrl+E took.
    compose: Mutex<Option<String>>,
    /// The buffer (split at the cursor) an idle Ctrl+Z took.
    suspend: Mutex<Option<(String, String)>>,
    /// The line is a secret: drawn as `*` and never remembered.
    masked: AtomicBool,
    /// The bottom status line's text (`None`: no status line).
    status: Mutex<Option<String>>,
    bottom: Mutex<super::bottom_line::BottomLine>,
}

/// How long [`Input::flush`] waits for earlier output.
const FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// What the printer thread is handed.
enum Output {
    Text(String),
    /// Answered once every earlier text is out.
    Flush(std_mpsc::Sender<()>),
}

/// The REPL's end of the input thread.
pub struct Input {
    requests: Option<std_mpsc::Sender<String>>,
    events: mpsc::UnboundedReceiver<Event>,
    output: Option<std_mpsc::Sender<Output>>,
    reading: bool,
    shared: Arc<Shared>,
    printing: Option<std::thread::JoinHandle<()>>,
}

/// Prints through the input's printer thread (above the prompt while a
/// line is being read).
#[derive(Clone)]
pub struct Printer(Option<std_mpsc::Sender<Output>>);

impl Printer {
    /// Prints `text` (one or more lines) after everything printed before.
    pub fn print(&self, text: &str) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        if let Some(output) = &self.0 {
            let _ = output.send(Output::Text(format!("{text}\n")));
        }
    }
}

/// What the editor needs.
pub struct Setup {
    /// `<profile>/cli_history`.
    pub history: PathBuf,
    /// The session's working directory (`@` completion).
    pub cwd: PathBuf,
    /// Scrubs secrets from history entries.
    pub redactor: Redactor,
    /// Vi key bindings instead of Emacs ones (`display.vim_mode`).
    pub vim_mode: bool,
}

impl Input {
    /// Starts the input thread.
    ///
    /// # Errors
    ///
    /// The terminal cannot be set up for line editing.
    pub fn start(setup: Setup) -> Result<Self, pmt_core::Error> {
        let (requests, wanted) = std_mpsc::channel::<String>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (printer_tx, printer_rx) = std_mpsc::channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("line-input".to_owned())
            .spawn(move || {
                editor_thread(&setup, &thread_shared, &wanted, &events_tx, &printer_tx);
            })
            .map_err(|e| io_error(&e.to_string()))?;
        let printer = printer_rx
            .recv()
            .map_err(|_| io_error("the line editor did not start"))?;
        let (output, lines) = std_mpsc::channel();
        let printing = std::thread::Builder::new()
            .name("line-output".to_owned())
            .spawn(move || printer_thread(printer, &lines))
            .map_err(|e| io_error(&e.to_string()))?;
        Ok(Self {
            requests: Some(requests),
            events,
            output: Some(output),
            reading: false,
            shared,
            printing: Some(printing),
        })
    }

    /// Reads the next lines hidden (a secret): echoed as `*`, kept out of
    /// the history.
    pub fn set_masked(&self, masked: bool) {
        self.shared.masked.store(masked, Ordering::Relaxed);
    }

    /// Sets the bottom status line drawn before each read (`None` gives
    /// the row back).
    pub fn set_status(&self, text: Option<String>) {
        *self
            .shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = text;
    }

    /// `/redraw`: clears the screen; the next read reprints the prompt and
    /// the status line.
    pub fn redraw(&self) {
        self.print("\x1b[r\x1b[2J\x1b[H");
    }

    /// Marks whether a turn runs (Ctrl+Z is refused meanwhile).
    pub fn set_busy(&self, busy: bool) {
        self.shared.busy.store(busy, Ordering::Relaxed);
    }

    /// Asks for the next line with `prompt` (once per line). Everything
    /// printed before is written first: the editor draws the prompt as
    /// soon as it gets the request, and a line still queued would land
    /// after the prompt instead of above it.
    pub fn request(&mut self, prompt: &str) {
        if self.reading {
            return;
        }
        self.flush();
        if let Some(requests) = &self.requests
            && requests.send(prompt.to_owned()).is_ok()
        {
            self.reading = true;
        }
    }

    /// The next event; [`Event::Eof`] when the editor is gone.
    pub async fn next(&mut self) -> Event {
        let event = self.events.recv().await.unwrap_or(Event::Eof);
        // A refused Ctrl+Z happens in the middle of a read.
        if event != Event::SuspendRefused {
            self.reading = false;
        }
        event
    }

    /// Prints `text` (one or more lines), after everything printed
    /// before it: above the prompt while a line is being read, else
    /// straight to the terminal. Never waits.
    pub fn print(&self, text: &str) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        if let Some(output) = &self.output {
            let _ = output.send(Output::Text(format!("{text}\n")));
        }
    }

    /// A handle that prints like [`Input::print`] from anywhere.
    #[must_use]
    pub fn printer(&self) -> Printer {
        Printer(self.output.clone())
    }

    /// Waits (at most [`FLUSH_WAIT`]) until everything printed so far is
    /// out: before the editor draws a prompt, and before a child process
    /// takes the terminal.
    pub fn flush(&self) {
        let (done, wait) = std_mpsc::channel();
        if let Some(output) = &self.output
            && output.send(Output::Flush(done)).is_ok()
        {
            let _ = wait.recv_timeout(FLUSH_WAIT);
        }
    }
}

impl Drop for Input {
    /// Ends the editor and waits until the printer has written what is
    /// left (the editor thread is not joined: it may be inside a read).
    fn drop(&mut self) {
        self.requests = None;
        self.output = None;
        if let Some(printing) = self.printing.take() {
            let _ = printing.join();
        }
        self.shared
            .bottom
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release();
    }
}

/// Writes every output in order: through the editor's printer (above a
/// running read, or straight out when none runs), else to stdout. A text
/// the editor refuses (it has ended) goes to stdout.
fn printer_thread(mut printer: PrinterSlot, lines: &std_mpsc::Receiver<Output>) {
    for output in lines {
        match output {
            Output::Text(text) => {
                let delivered = printer
                    .as_mut()
                    .is_some_and(|p| p.print(text.clone()).is_ok());
                if !delivered {
                    printer = None;
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                }
            }
            Output::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn io_error(message: &str) -> pmt_core::Error {
    pmt_core::Error::new(&pmt_core::error::codes::OUTPUT_IO, message.to_owned())
}

type PrinterSlot = Option<Box<dyn ExternalPrinter + Send>>;

fn editor_thread(
    setup: &Setup,
    shared: &Arc<Shared>,
    wanted: &std_mpsc::Receiver<String>,
    events: &mpsc::UnboundedSender<Event>,
    printer: &std_mpsc::Sender<PrinterSlot>,
) {
    let mut editor = match editor(setup, shared, events) {
        Ok(editor) => editor,
        Err(err) => {
            tracing::warn!(error = %err, "line editor unavailable");
            let _ = printer.send(None);
            let _ = events.send(Event::Eof);
            return;
        }
    };
    let external = editor
        .create_external_printer()
        .ok()
        .map(|p| Box::new(p) as Box<dyn ExternalPrinter + Send>);
    let _ = printer.send(external);
    while let Ok(prompt) = wanted.recv() {
        let status = shared
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        shared
            .bottom
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .show(status.as_deref());
        let event = match read_line(&mut editor, shared, &prompt) {
            Ok(line) if shared.masked.load(Ordering::Relaxed) => Event::Line(line),
            Ok(line) => {
                remember(&mut editor, setup, &line);
                Event::Line(join_continuations(&line))
            }
            Err(ReadlineError::Interrupted) => {
                let composed = shared
                    .compose
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                composed.map_or(Event::Interrupt, Event::Compose)
            }
            Err(_) => Event::Eof,
        };
        let eof = event == Event::Eof;
        if events.send(event).is_err() || eof {
            return;
        }
    }
}

fn editor(
    setup: &Setup,
    shared: &Arc<Shared>,
    events: &mpsc::UnboundedSender<Event>,
) -> rustyline::Result<Editor<LineHelper, FileHistory>> {
    let config = Config::builder()
        .max_history_size(HISTORY_CAP)?
        .history_ignore_dups(true)?
        .history_ignore_space(true)
        .auto_add_history(false)
        // The helper only masks a secret (it adds no colors); rustyline
        // calls it only with colors on, which NO_COLOR would turn off.
        .color_mode(ColorMode::Forced)
        .edit_mode(if setup.vim_mode {
            EditMode::Vi
        } else {
            EditMode::Emacs
        })
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(LineHelper {
        cwd: setup.cwd.clone(),
        shared: Arc::clone(shared),
    }));
    editor.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
        EventHandler::Simple(Cmd::Newline),
    );
    editor.bind_sequence(
        rustyline::Event::KeySeq(vec![KeyEvent::ctrl('X'), KeyEvent::ctrl('E')]),
        EventHandler::Conditional(Box::new(ComposeKey(Arc::clone(shared)))),
    );
    editor.bind_sequence(
        KeyEvent::ctrl('Z'),
        EventHandler::Conditional(Box::new(SuspendKey {
            shared: Arc::clone(shared),
            events: events.clone(),
        })),
    );
    prepare_history_file(&setup.history);
    let _ = editor.load_history(&setup.history);
    Ok(editor)
}

/// Creates the history file owner-only before the editor ever writes it
/// (later writes keep the mode).
fn prepare_history_file(path: &Path) {
    if path.exists() {
        return;
    }
    if let Err(err) = pmt_core::private_fs::new_file_options().open(path) {
        tracing::warn!(error = %err, path = %path.display(), "cannot create the history file");
    }
}

/// Adds `line` to the history, secrets redacted, and appends it to the
/// file (rewritten at the cap).
fn remember(editor: &mut Editor<LineHelper, FileHistory>, setup: &Setup, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let redacted = setup.redactor.redact_all(line);
    if !matches!(editor.add_history_entry(redacted.as_str()), Ok(true)) {
        return;
    }
    let result = if editor.history().len() >= HISTORY_CAP {
        editor.save_history(&setup.history)
    } else {
        editor.append_history(&setup.history)
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, "cannot write the history file");
    }
}

/// One line: after an idle Ctrl+Z the process stops, and on `fg` the read
/// starts over with the buffer it had.
fn read_line(
    editor: &mut Editor<LineHelper, FileHistory>,
    shared: &Shared,
    prompt: &str,
) -> rustyline::Result<String> {
    let mut initial = (String::new(), String::new());
    loop {
        let result = editor.readline_with_initial(prompt, (&initial.0, &initial.1));
        let suspended = shared
            .suspend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match (result, suspended) {
            (Err(ReadlineError::Interrupted), Some(buffer)) => {
                stop_process();
                initial = buffer;
            }
            (result, _) => return result,
        }
    }
}

/// Stops the process (job control) until it is continued. Raised on the
/// calling thread so it takes effect before this thread returns.
#[cfg(unix)]
fn stop_process() {
    use nix::sys::signal::{Signal, raise};
    if let Err(err) = raise(Signal::SIGTSTP) {
        tracing::warn!(error = %err, "cannot suspend");
    }
}

#[cfg(not(unix))]
fn stop_process() {}

/// Ctrl+X Ctrl+E: keep the buffer for the editor and end the read (as an
/// interrupt, so the line is neither validated nor remembered).
struct ComposeKey(Arc<Shared>);

impl ConditionalEventHandler for ComposeKey {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        *self
            .0
            .compose
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ctx.line().to_owned());
        Some(Cmd::Interrupt)
    }
}

/// Ctrl+Z: suspends when idle (see [`read_line`]); refused while busy.
struct SuspendKey {
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<Event>,
}

impl ConditionalEventHandler for SuspendKey {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if self.shared.busy.load(Ordering::Relaxed) {
            let _ = self.events.send(Event::SuspendRefused);
            return Some(Cmd::Noop);
        }
        if !cfg!(unix) {
            return None;
        }
        // End the read (as an interrupt: the line is neither validated nor
        // remembered); `read_line` stops the process and reads again.
        let (left, right) = ctx.line().split_at(ctx.pos());
        *self
            .shared
            .suspend
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((left.to_owned(), right.to_owned()));
        Some(Cmd::Interrupt)
    }
}

/// A trailing `\` before a newline continues the line: drop the `\`.
#[must_use]
pub fn join_continuations(line: &str) -> String {
    line.replace("\\\n", "\n")
}

/// Completion, multi-line validation, masking of a secret.
struct LineHelper {
    cwd: PathBuf,
    shared: Arc<Shared>,
}

impl LineHelper {
    fn masked(&self) -> bool {
        self.shared.masked.load(Ordering::Relaxed)
    }
}

impl Completer for LineHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        });
        let word = &before[start..];
        if self.masked() || !word.starts_with('@') {
            return Ok((pos, Vec::new()));
        }
        let candidates = pmt_agent::references::complete::complete(word, &self.cwd)
            .into_iter()
            .map(|c| Pair {
                display: c.display,
                replacement: c.text,
            })
            .collect();
        Ok((start, candidates))
    }
}

impl Validator for LineHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(if !self.masked() && ctx.input().ends_with('\\') {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl Hinter for LineHelper {
    type Hint = String;
}

impl Highlighter for LineHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.masked() {
            Cow::Owned("*".repeat(line.chars().count()))
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_char(
        &self,
        _line: &str,
        _pos: usize,
        _kind: rustyline::highlight::CmdKind,
    ) -> bool {
        self.masked()
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}

impl Helper for LineHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        assert_eq!(join_continuations("one \\\ntwo"), "one \ntwo");
        assert_eq!(join_continuations("a\\b"), "a\\b");
    }
}
