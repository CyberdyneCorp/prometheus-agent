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

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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
    Cmd, Config, Editor, EventHandler, ExternalPrinter, Helper, KeyCode, KeyEvent, Modifiers,
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
}

/// The REPL's end of the input thread.
pub struct Input {
    requests: std_mpsc::Sender<String>,
    events: mpsc::UnboundedReceiver<Event>,
    printer: Option<Arc<Mutex<Box<dyn ExternalPrinter + Send>>>>,
    reading: bool,
}

/// What the editor needs.
pub struct Setup {
    /// `<profile>/cli_history`.
    pub history: PathBuf,
    /// The session's working directory (`@` completion).
    pub cwd: PathBuf,
    /// Scrubs secrets from history entries.
    pub redactor: Redactor,
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
        std::thread::Builder::new()
            .name("line-input".to_owned())
            .spawn(move || editor_thread(&setup, &wanted, &events_tx, &printer_tx))
            .map_err(|e| io_error(&e.to_string()))?;
        let printer = printer_rx
            .recv()
            .map_err(|_| io_error("the line editor did not start"))?
            .map(|p| Arc::new(Mutex::new(p)));
        Ok(Self {
            requests,
            events,
            printer,
            reading: false,
        })
    }

    /// Asks for the next line with `prompt` (once per line).
    pub fn request(&mut self, prompt: &str) {
        if !self.reading && self.requests.send(prompt.to_owned()).is_ok() {
            self.reading = true;
        }
    }

    /// The next event; [`Event::Eof`] when the editor is gone.
    pub async fn next(&mut self) -> Event {
        let event = self.events.recv().await.unwrap_or(Event::Eof);
        self.reading = false;
        event
    }

    /// Prints `text` (one or more lines): above the prompt while a line is
    /// being read, else straight to stdout.
    pub fn print(&self, text: &str) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        if let (true, Some(printer)) = (self.reading, &self.printer) {
            let mut printer = printer.lock().unwrap_or_else(PoisonError::into_inner);
            if printer.print(format!("{text}\n")).is_ok() {
                return;
            }
        }
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
}

fn io_error(message: &str) -> pmt_core::Error {
    pmt_core::Error::new(&pmt_core::error::codes::OUTPUT_IO, message.to_owned())
}

type PrinterSlot = Option<Box<dyn ExternalPrinter + Send>>;

fn editor_thread(
    setup: &Setup,
    wanted: &std_mpsc::Receiver<String>,
    events: &mpsc::UnboundedSender<Event>,
    printer: &std_mpsc::Sender<PrinterSlot>,
) {
    let mut editor = match editor(setup) {
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
        let event = match editor.readline(&prompt) {
            Ok(line) => {
                remember(&mut editor, setup, &line);
                Event::Line(join_continuations(&line))
            }
            Err(ReadlineError::Interrupted) => Event::Interrupt,
            Err(_) => Event::Eof,
        };
        let eof = event == Event::Eof;
        if events.send(event).is_err() || eof {
            return;
        }
    }
}

fn editor(setup: &Setup) -> rustyline::Result<Editor<LineHelper, FileHistory>> {
    let config = Config::builder()
        .max_history_size(HISTORY_CAP)?
        .history_ignore_dups(true)?
        .history_ignore_space(true)
        .auto_add_history(false)
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(LineHelper {
        cwd: setup.cwd.clone(),
    }));
    editor.bind_sequence(
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
        EventHandler::Simple(Cmd::Newline),
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

/// A trailing `\` before a newline continues the line: drop the `\`.
#[must_use]
pub fn join_continuations(line: &str) -> String {
    line.replace("\\\n", "\n")
}

/// Completion, multi-line validation.
struct LineHelper {
    cwd: PathBuf,
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
        if !word.starts_with('@') {
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
        Ok(if ctx.input().ends_with('\\') {
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
