//! The file tools: `read_file`, `write_file`, `patch`, `search_files` and
//! the `list_dir` backend operation (spec: execution-environments, File
//! tools route through the backend and the requirements after it).
//!
//! A [`FileSession`] is one session's view: its [`FileBackend`], working
//! directory, guards and the [`tracker::Tracker`] behind read dedup and
//! stale-write protection. Each tool is a method returning the typed
//! [`ToolResult`] the model sees; [`tools`] turns them into registered
//! tools that find the session through [`crate::environments`].

pub mod backend;
mod diff;
pub mod guards;
pub mod lint;
pub mod patch;
mod read;
pub mod search;
pub mod text;
pub mod tools;
pub mod tracker;
mod write;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;

pub use backend::{Entry, FileBackend, Kind, LocalBackend, Meta};
pub use guards::ControlPaths;
pub use lint::Linter;
pub use read::{DEFAULT_LIMIT, ReadArgs};

use crate::result::{Status, ToolResult};
use tracker::Tracker;

/// The session's current working directory (the terminal's, when the
/// session has one, so relative paths follow `cd`).
pub type CwdSource = Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// What a [`FileSession`] is built from.
pub struct FileSetup {
    /// The session's execution backend.
    pub backend: Arc<dyn FileBackend>,
    /// Where relative paths resolve.
    pub cwd: CwdSource,
    /// `~` for path expansion.
    pub user_home: Option<PathBuf>,
    /// Prometheus' own state, for the write guard.
    pub control: ControlPaths,
    /// `files.read_max_chars`.
    pub read_max_chars: usize,
    /// Post-write syntax checks.
    pub linter: Linter,
}

/// One session's file tools.
pub struct FileSession {
    setup: FileSetup,
    tracker: Mutex<Tracker>,
    generation: AtomicU64,
}

impl FileSession {
    /// A session over `setup`.
    #[must_use]
    pub fn new(setup: FileSetup) -> Self {
        Self {
            setup,
            tracker: Mutex::default(),
            generation: AtomicU64::new(0),
        }
    }

    /// Context compaction happened: earlier read results may be gone from
    /// the model's context, so identical reads are answered in full again.
    pub fn note_compaction(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// `raw` as an absolute path in the backend's namespace.
    #[must_use]
    pub fn resolve(&self, raw: &str) -> PathBuf {
        guards::absolute(raw, &(self.setup.cwd)(), self.setup.user_home.as_deref())
    }

    /// The working directory relative paths resolve against.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        (self.setup.cwd)()
    }

    /// The entries of a directory (the `list_dir` operation).
    ///
    /// # Errors
    ///
    /// The refusal or I/O failure as text.
    pub fn list_dir(&self, raw: &str) -> Result<Vec<Entry>, String> {
        if let Some(refusal) = guards::device_namespace(raw) {
            return Err(refusal);
        }
        self.setup
            .backend
            .list_dir(&self.resolve(raw))
            .map_err(|err| io_message(raw, &err))
    }

    /// The bytes of a file (images for `vision_analyze`), under the same
    /// guards as `read_file`: device paths, protected secret files, file
    /// kind and the backend's capability checks.
    ///
    /// # Errors
    ///
    /// The refusal or I/O failure as text.
    pub fn read_binary(&self, raw: &str) -> Result<Vec<u8>, String> {
        if let Some(refusal) = guards::device_namespace(raw) {
            return Err(refusal);
        }
        let path = self.resolve(raw);
        if let Some(refusal) = guards::read_refusal(&path, raw) {
            return Err(refusal);
        }
        self.read_bytes(&path, raw)
            .map_err(|result| result.text_content())
    }

    fn backend(&self) -> &dyn FileBackend {
        self.setup.backend.as_ref()
    }

    fn tracker(&self) -> MutexGuard<'_, Tracker> {
        self.tracker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// A JSON object result of `status`.
pub(crate) fn json_result(status: Status, value: &Value) -> ToolResult {
    ToolResult::text(status, value.to_string())
}

/// The result of an I/O failure: a capability refusal is `denied`.
pub(crate) fn io_failure(shown: &str, err: &io::Error) -> ToolResult {
    let status = if err.kind() == io::ErrorKind::PermissionDenied {
        Status::Denied
    } else {
        Status::Error
    };
    ToolResult::text(status, io_message(shown, err))
}

pub(crate) fn io_message(shown: &str, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => format!("File not found: {shown}"),
        _ => format!("{shown}: {err}"),
    }
}

/// Lines of `text` as the tools number them: split on `\n`, a final
/// newline does not start another line, `\r` before `\n` is dropped.
pub(crate) fn lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() && text.len() <= 1 {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![""]
        };
    }
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// `path` relative to `base` when below it, for display.
/// Separators are shown as `/` on every platform (Windows accepts them).
pub(crate) fn shown(path: &Path, base: &Path) -> String {
    let text = path.strip_prefix(base).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    );
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}
