//! The trajectory writer (spec: observability, Trajectories are lossless
//! and redacted; Trajectory retention and size limits).
//!
//! Producers hand events to a bounded queue drained by one writer thread.
//! A full queue ([`MAX_PENDING`] events) makes [`TrajectoryWriter::record`]
//! wait: the turn slows down, no event is ever dropped. The thread assigns
//! `seq`, redacts every string of the payload (known secret values become
//! `[REDACTED:<NAME>]`, credential-shaped tokens `[REDACTED]`) before
//! anything reaches the 0600 file, and once the file would exceed the
//! session's byte limit it writes one `error` event with code
//! `trajectory.size_limit` and from then on records events without their
//! payload. [`TrajectoryWriter::finish`] (session end) compresses the file.

use std::fs::File;
use std::io::{self, BufRead as _, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::secret::Redactor;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::payload::ErrorEvent;
use super::{Event, Line, OMITTED_KEY, files, new_span_id, timestamp, version};

/// Events queued before producers wait.
pub const MAX_PENDING: usize = 10_000;

/// What a session's writer needs.
#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// `<profile>/trajectories`.
    pub dir: PathBuf,
    /// The session.
    pub session_id: String,
    /// Its lineage root.
    pub root_session_id: String,
    /// `observability.trajectories.max_bytes_per_session`.
    pub max_bytes: u64,
    /// The profile's known secrets (`scope.secrets().redactor()`); `None`
    /// only when `security.redact_secrets = false`.
    pub redactor: Option<Redactor>,
}

/// Opens the trajectory of a session of `scope` that starts now, or `None`
/// when `observability.trajectories.enabled` is false. The setting is read
/// once here, so changing it only affects sessions started afterwards.
///
/// # Errors
///
/// Secret store errors (building the redactor); as
/// [`TrajectoryWriter::open`].
pub fn open_for_session(
    scope: &pmt_config::scope::ProfileScope,
    session_id: &str,
    root_session_id: &str,
) -> Result<Option<TrajectoryWriter>, Error> {
    let config = scope.config();
    let settings = &config.observability.trajectories;
    if !settings.enabled {
        return Ok(None);
    }
    let redactor = if config.security.redact_secrets {
        Some(scope.secrets().redactor()?)
    } else {
        None
    };
    TrajectoryWriter::open(WriterOptions {
        dir: scope.path(files::DIR),
        session_id: session_id.to_owned(),
        root_session_id: root_session_id.to_owned(),
        max_bytes: settings.max_bytes_per_session,
        redactor,
    })
    .map(Some)
}

/// One event to record, with its header fields; `seq` is assigned by the
/// writer in queue order.
#[derive(Debug, Clone, PartialEq)]
pub struct Emit {
    /// When it happened.
    pub ts: jiff::Timestamp,
    /// The turn, if inside one.
    pub turn_id: Option<String>,
    /// This event's span.
    pub span_id: String,
    /// The enclosing span.
    pub parent_span_id: Option<String>,
    /// The event.
    pub event: Event,
}

impl Emit {
    /// `event` now, in a new span, outside any turn.
    #[must_use]
    pub fn now(event: Event) -> Self {
        Self {
            ts: jiff::Timestamp::now(),
            turn_id: None,
            span_id: new_span_id(),
            parent_span_id: None,
            event,
        }
    }

    /// The same event inside `turn_id`, under `parent_span_id`.
    #[must_use]
    pub fn in_turn(mut self, turn_id: &str, parent_span_id: Option<&str>) -> Self {
        self.turn_id = Some(turn_id.to_owned());
        self.parent_span_id = parent_span_id.map(str::to_owned);
        self
    }
}

/// A session's trajectory writer.
#[derive(Debug)]
pub struct TrajectoryWriter {
    path: PathBuf,
    sender: Option<mpsc::Sender<Emit>>,
    worker: Option<JoinHandle<Result<(), Error>>>,
}

impl TrajectoryWriter {
    /// Opens (or, for a reopened session, continues) the session's
    /// trajectory and starts the writer thread.
    ///
    /// # Errors
    ///
    /// `trajectory.io` when the directory or file cannot be created, or an
    /// existing trajectory cannot be read.
    pub fn open(options: WriterOptions) -> Result<Self, Error> {
        Self::with_capacity(options, MAX_PENDING)
    }

    /// [`TrajectoryWriter::open`] with another queue bound (tests of the
    /// backpressure path).
    ///
    /// # Errors
    ///
    /// As [`TrajectoryWriter::open`].
    pub fn with_capacity(options: WriterOptions, capacity: usize) -> Result<Self, Error> {
        let (worker, path) = Worker::open(options).map_err(|err| io_error(&err))?;
        let (sender, receiver) = mpsc::channel(capacity);
        let thread = std::thread::Builder::new()
            .name("trajectory-writer".to_owned())
            .spawn(move || worker.run(receiver))
            .map_err(|err| io_error(&err))?;
        Ok(Self {
            path,
            sender: Some(sender),
            worker: Some(thread),
        })
    }

    /// The file being written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues `emit`, waiting while [`MAX_PENDING`] events are pending.
    ///
    /// # Errors
    ///
    /// `trajectory.io` when the writer stopped after an I/O error
    /// ([`TrajectoryWriter::finish`] returns the cause).
    pub async fn record(&self, emit: Emit) -> Result<(), Error> {
        match &self.sender {
            Some(sender) => sender.send(emit).await.map_err(|_| stopped()),
            None => Err(stopped()),
        }
    }

    /// [`TrajectoryWriter::record`] for synchronous callers. Must not be
    /// called from inside an async runtime.
    ///
    /// # Errors
    ///
    /// As [`TrajectoryWriter::record`].
    pub fn blocking_record(&self, emit: Emit) -> Result<(), Error> {
        match &self.sender {
            Some(sender) => sender.blocking_send(emit).map_err(|_| stopped()),
            None => Err(stopped()),
        }
    }

    /// Writes every pending event and stops the thread, keeping the plain
    /// file (for a session that is not over, such as a process exit).
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the writer, if any.
    pub fn close(mut self) -> Result<PathBuf, Error> {
        self.stop()?;
        Ok(self.path.clone())
    }

    /// Session end: writes every pending event, then compresses the file
    /// to `<session_id>.jsonl.zst`, whose path it returns.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the writer; `trajectory.io` when
    /// compression fails (the plain file is kept).
    pub fn finish(mut self) -> Result<PathBuf, Error> {
        self.stop()?;
        files::compress(&self.path).map_err(|err| io_error(&err))
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.sender = None;
        match self.worker.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(stopped()),
            None => Ok(()),
        }
    }
}

impl Drop for TrajectoryWriter {
    fn drop(&mut self) {
        // Flush what is queued; an error here has no one left to report to.
        let _ = self.stop();
    }
}

/// The writer thread's state.
struct Worker {
    file: BufWriter<File>,
    session_id: String,
    root_session_id: String,
    redactor: Option<Redactor>,
    max_bytes: u64,
    seq: u64,
    written: u64,
    capped: bool,
}

impl Worker {
    fn open(options: WriterOptions) -> io::Result<(Self, PathBuf)> {
        private_fs::create_dir(&options.dir)?;
        let path = files::plain(&options.dir, &options.session_id);
        let compressed = files::compressed(&options.dir, &options.session_id);
        if !path.exists() && compressed.exists() {
            files::decompress(&compressed)?;
        }
        let (seq, written) = if path.exists() {
            existing(&path)?
        } else {
            (0, 0)
        };
        let file = private_fs::append_options().open(&path)?;
        // A file left by an older build or copied in keeps no looser mode.
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let worker = Self {
            file: BufWriter::new(file),
            session_id: options.session_id,
            root_session_id: options.root_session_id,
            redactor: options.redactor,
            max_bytes: options.max_bytes,
            seq,
            written,
            capped: written > options.max_bytes,
        };
        Ok((worker, path))
    }

    fn run(mut self, mut receiver: mpsc::Receiver<Emit>) -> Result<(), Error> {
        let result = (|| {
            while let Some(emit) = receiver.blocking_recv() {
                self.write(&emit)?;
                if receiver.is_empty() {
                    self.file.flush()?;
                }
            }
            self.file.flush()?;
            self.file.get_ref().sync_all()
        })();
        result.map_err(|err| io_error(&err))
    }

    fn write(&mut self, emit: &Emit) -> io::Result<()> {
        let kind = emit.event.kind();
        let payload = match &self.redactor {
            Some(redactor) => redact(emit.event.payload(), redactor),
            None => emit.event.payload(),
        };
        let full = self.line(emit, kind, payload.clone());
        if !self.capped && self.written + full.len() as u64 > self.max_bytes {
            self.capped = true;
            let limit = self.size_limit_event(emit);
            self.append(&limit)?;
        }
        if self.capped {
            let omitted = json!({ OMITTED_KEY: payload.to_string().len() });
            let header = self.line(emit, kind, omitted);
            return self.append(&header);
        }
        self.append(&full)
    }

    fn size_limit_event(&self, emit: &Emit) -> String {
        let event = Event::Error(ErrorEvent {
            code: codes::TRAJECTORY_SIZE_LIMIT.code.to_owned(),
            kind: "io".to_owned(),
            message: format!(
                "trajectory reached {} bytes (observability.trajectories.max_bytes_per_session); \
                 later events are recorded without their payload",
                self.max_bytes
            ),
        });
        self.line(emit, event.kind(), event.payload())
    }

    /// One serialized line (with its newline) carrying the next `seq`.
    fn line(&self, emit: &Emit, kind: &str, payload: Value) -> String {
        let line = Line {
            v: version(),
            ts: timestamp(emit.ts),
            seq: self.seq + 1,
            session_id: self.session_id.clone(),
            root_session_id: self.root_session_id.clone(),
            turn_id: emit.turn_id.clone(),
            span_id: emit.span_id.clone(),
            parent_span_id: emit.parent_span_id.clone(),
            kind: kind.to_owned(),
            payload,
        };
        let mut text = serde_json::to_string(&line).unwrap_or_default();
        text.push('\n');
        text
    }

    fn append(&mut self, line: &str) -> io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.seq += 1;
        self.written += line.len() as u64;
        Ok(())
    }
}

/// The last `seq` and the size of an existing trajectory.
fn existing(path: &Path) -> io::Result<(u64, u64)> {
    #[derive(serde::Deserialize)]
    struct Seq {
        seq: u64,
    }
    let size = std::fs::metadata(path)?.len();
    let mut last = 0;
    for line in io::BufReader::new(File::open(path)?).lines() {
        if let Ok(Seq { seq }) = serde_json::from_str(&line?) {
            last = last.max(seq);
        }
    }
    Ok((last, size))
}

/// `value` with every string redacted (secret values become
/// `[REDACTED:<NAME>]`, credential-shaped tokens `[REDACTED]`).
fn redact(value: Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(text) => Value::String(redactor.redact_all(&text)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| redact(v, redactor)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, v)| (key, redact(v, redactor)))
                .collect(),
        ),
        other => other,
    }
}

fn io_error(err: &io::Error) -> Error {
    Error::new(
        &codes::TRAJECTORY_IO,
        format!("cannot write the trajectory: {err}"),
    )
}

fn stopped() -> Error {
    Error::new(
        &codes::TRAJECTORY_IO,
        "the trajectory writer stopped after an error",
    )
}
