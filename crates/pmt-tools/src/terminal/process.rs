//! The background process registry behind the `process` tool (spec:
//! execution-environments, Background process registry).
//!
//! Each session tracks at most [`MAX_PROCESSES`] processes; adding one more
//! prunes the oldest finished one. Every process keeps a rolling
//! [`BUFFER_CHARS`]-character output buffer and stays queryable until
//! [`EXPIRY`] after it *finished*. [`ProcessRegistry::close`] (session end)
//! stops every process group, and dropping the registry kills whatever is
//! left, so a closed session leaves no orphan behind.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::secret::Redactor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::shape;
use super::spawn::{self, Group, Spawned};

/// Processes tracked per session.
pub const MAX_PROCESSES: usize = 64;
/// Characters of output kept per process.
pub const BUFFER_CHARS: usize = 200_000;
/// How long a finished process stays queryable.
pub const EXPIRY: Duration = Duration::from_mins(30);
/// The `process` tool's actions.
pub const ACTIONS: [&str; 9] = [
    "list", "poll", "log", "wait", "kill", "write", "submit", "close", "handoff",
];

/// The id prefix of background processes.
const ID_PREFIX: &str = "proc_";
/// How long output readers may lag behind the process exit.
const READ_GRACE: Duration = Duration::from_secs(1);

/// The time source (replaceable in tests).
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// A finished process whose caller asked to be told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completion {
    /// Its id.
    pub process_id: String,
    /// Its command line.
    pub command: String,
    /// Its exit code (128 + signal for a signal death).
    pub exit_code: i32,
}

/// Registry settings.
#[derive(Clone)]
pub struct Options {
    /// SIGTERM-to-SIGKILL wait (`tools.kill_grace_secs`).
    pub kill_grace: Duration,
    /// Characters returned per call (`tool_output.max_chars`).
    pub max_chars: usize,
    /// Scrubs output before it is returned.
    pub redactor: Redactor,
    /// Where completion notifications go.
    pub notify: Option<mpsc::UnboundedSender<Completion>>,
    /// The time source.
    pub clock: Clock,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("kill_grace", &self.kill_grace)
            .field("max_chars", &self.max_chars)
            .finish_non_exhaustive()
    }
}

/// One `process` call.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessArgs {
    /// One of [`ACTIONS`].
    pub action: String,
    /// The process id or a unique prefix of it.
    pub process_id: Option<String>,
    /// Text for `write` / `submit`.
    pub data: Option<String>,
    /// Seconds `wait` waits at most.
    pub timeout: Option<f64>,
}

/// The rolling output of one process.
#[derive(Debug, Default)]
struct Output {
    text: String,
    chars: usize,
    /// Characters ever received.
    total: usize,
    /// `total` at the last `poll`.
    polled: usize,
    /// An incomplete UTF-8 sequence at the end of the last chunk.
    carry: Vec<u8>,
}

impl Output {
    fn push(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        // Hold back only an incomplete sequence at the very end.
        let valid = match std::str::from_utf8(&self.carry) {
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            _ => self.carry.len(),
        };
        let chunk: Vec<u8> = self.carry.drain(..valid).collect();
        let text = String::from_utf8_lossy(&chunk);
        let added = text.chars().count();
        self.text.push_str(&text);
        self.chars += added;
        self.total += added;
        if self.chars > BUFFER_CHARS {
            let excess = self.chars - BUFFER_CHARS;
            let cut = self
                .text
                .char_indices()
                .nth(excess)
                .map_or(self.text.len(), |(i, _)| i);
            self.text.drain(..cut);
            self.chars = BUFFER_CHARS;
        }
    }

    /// Output received after character `mark` (as much as is still kept).
    fn since(&self, mark: usize) -> &str {
        let first_kept = self.total - self.chars;
        let skip = mark.saturating_sub(first_kept);
        let at = self
            .text
            .char_indices()
            .nth(skip)
            .map_or(self.text.len(), |(i, _)| i);
        &self.text[at..]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Finished {
        exit_code: i32,
        at: Instant,
        killed: bool,
    },
}

struct Entry {
    id: String,
    command: String,
    started: Instant,
    pid: Option<u32>,
    group: Group,
    notify: bool,
    output: Mutex<Output>,
    state: watch::Sender<State>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

impl Entry {
    fn output(&self) -> MutexGuard<'_, Output> {
        self.output.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> State {
        *self.state.borrow()
    }
}

/// A session's background processes.
pub struct ProcessRegistry {
    options: Options,
    entries: Mutex<Vec<Arc<Entry>>>,
    supervisors: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for ProcessRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRegistry")
            .field("processes", &self.lock().len())
            .finish_non_exhaustive()
    }
}

impl ProcessRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new(options: Options) -> Self {
        Self {
            options,
            entries: Mutex::new(Vec::new()),
            supervisors: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Arc<Entry>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Instant {
        (self.options.clock)()
    }

    /// Drops processes that finished more than [`EXPIRY`] ago.
    fn expire(&self) {
        let now = self.now();
        self.lock().retain(|entry| match entry.state() {
            State::Running => true,
            State::Finished { at, .. } => now.saturating_duration_since(at) < EXPIRY,
        });
    }

    /// Makes room for one more process.
    ///
    /// # Errors
    ///
    /// The message the result carries when [`MAX_PROCESSES`] are running.
    pub fn reserve(&self) -> Result<(), String> {
        self.expire();
        let mut entries = self.lock();
        if entries.len() < MAX_PROCESSES {
            return Ok(());
        }
        let oldest = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e.state() {
                State::Finished { at, .. } => Some((i, at)),
                State::Running => None,
            })
            .min_by_key(|(_, at)| *at)
            .map(|(i, _)| i);
        match oldest {
            Some(index) => {
                entries.remove(index);
                Ok(())
            }
            None => Err(format!(
                "{MAX_PROCESSES} background processes are running in this session; kill or wait for one first"
            )),
        }
    }

    /// Tracks a started process and returns its id. Call
    /// [`ProcessRegistry::reserve`] before starting it.
    pub fn track(&self, spawned: Spawned, command: &str, notify: bool) -> String {
        let Spawned { mut child, group } = spawned;
        let id = format!(
            "{ID_PREFIX}{}",
            ulid::Ulid::generate().to_string().to_lowercase()
        );
        let (kill_tx, kill_rx) = oneshot::channel();
        let entry = Arc::new(Entry {
            id: id.clone(),
            command: command.to_owned(),
            started: self.now(),
            pid: child.id(),
            group,
            notify,
            output: Mutex::default(),
            state: watch::Sender::new(State::Running),
            stdin: tokio::sync::Mutex::new(child.stdin.take()),
            kill: Mutex::new(Some(kill_tx)),
        });
        let readers: Vec<JoinHandle<()>> = [
            child.stdout.take().map(|r| reader(&entry, r)),
            child.stderr.take().map(|r| reader(&entry, r)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let supervisor = tokio::spawn(supervise(
            Arc::clone(&entry),
            child,
            readers,
            kill_rx,
            self.options.clone(),
        ));
        self.lock().push(entry);
        self.supervisors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(supervisor);
        id
    }

    /// Runs one `process` call and returns its JSON result.
    pub async fn handle(&self, args: &ProcessArgs) -> Value {
        self.expire();
        let action = args.action.as_str();
        if !ACTIONS.contains(&action) {
            return error(&format!(
                "unknown process action: {action}; use one of {}",
                ACTIONS.join(", ")
            ));
        }
        if action == "list" {
            return self.list();
        }
        let Some(prefix) = args.process_id.as_deref().filter(|p| !p.is_empty()) else {
            return error(&format!("process_id is required for {action}"));
        };
        let entry = match self.find(prefix) {
            Ok(entry) => entry,
            Err(message) => return error(&message),
        };
        match action {
            "poll" => self.poll(&entry),
            "log" => self.log(&entry),
            "wait" => self.wait(&entry, args.timeout).await,
            "kill" => self.kill(&entry).await,
            "write" | "submit" => {
                write(
                    &entry,
                    args.data.as_deref().unwrap_or(""),
                    action == "submit",
                )
                .await
            }
            "close" => close_stdin(&entry).await,
            _ => error("process handoff to a subagent is available in M2"),
        }
    }

    fn find(&self, prefix: &str) -> Result<Arc<Entry>, String> {
        let entries = self.lock();
        let matches: Vec<&Arc<Entry>> = entries
            .iter()
            .filter(|e| e.id.starts_with(prefix) || e.id[ID_PREFIX.len()..].starts_with(prefix))
            .collect();
        match matches.as_slice() {
            [one] => Ok(Arc::clone(one)),
            [] => Err(format!(
                "no process matches '{prefix}'; list them with action=list"
            )),
            many => Err(format!(
                "'{prefix}' matches several processes: {}",
                many.iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn list(&self) -> Value {
        let now = self.now();
        let processes: Vec<Value> = self
            .lock()
            .iter()
            .map(|e| {
                let mut item = describe(e);
                item["command"] = json!(e.command);
                item["pid"] = json!(e.pid);
                item["started_secs_ago"] =
                    json!(now.saturating_duration_since(e.started).as_secs());
                item
            })
            .collect();
        json!({"status": "ok", "processes": processes})
    }

    fn shaped(&self, text: &str) -> String {
        let clean = shape::clean(text, &[], &self.options.redactor);
        shape::truncate(&clean, self.options.max_chars).0
    }

    fn poll(&self, entry: &Entry) -> Value {
        let mut output = entry.output();
        let new = self.shaped(output.since(output.polled));
        output.polled = output.total;
        drop(output);
        let mut result = describe(entry);
        result["output"] = json!(new);
        result
    }

    fn log(&self, entry: &Entry) -> Value {
        let text = self.shaped(&entry.output().text);
        let mut result = describe(entry);
        result["output"] = json!(text);
        result
    }

    async fn wait(&self, entry: &Entry, timeout: Option<f64>) -> Value {
        let limit = Duration::from_secs_f64(
            timeout
                .filter(|t| *t > 0.0 && t.is_finite())
                .unwrap_or(180.0),
        );
        let mut state = entry.state.subscribe();
        let finished = tokio::time::timeout(limit, state.wait_for(|s| *s != State::Running)).await;
        let mut result = self.poll(entry);
        if finished.is_err() {
            result["note"] = json!(format!("still running after {}s", limit.as_secs()));
        }
        result
    }

    async fn kill(&self, entry: &Entry) -> Value {
        let sender = entry
            .kill
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
            let mut state = entry.state.subscribe();
            let limit = self.options.kill_grace + Duration::from_secs(5);
            let _ = tokio::time::timeout(limit, state.wait_for(|s| *s != State::Running)).await;
        }
        describe(entry)
    }

    /// Ids of processes still running.
    #[must_use]
    pub fn running(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|e| e.state() == State::Running)
            .map(|e| e.id.clone())
            .collect()
    }

    /// Session end: stops every process group and waits for them.
    pub async fn close(&self) {
        for entry in self.lock().iter() {
            if let Some(sender) = entry
                .kill
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let _ = sender.send(());
            }
        }
        let supervisors = std::mem::take(
            &mut *self
                .supervisors
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let limit = self.options.kill_grace + Duration::from_secs(5);
        for supervisor in supervisors {
            let _ = tokio::time::timeout(limit, supervisor).await;
        }
        // Stragglers that left the leader's group alive.
        for entry in self.lock().iter() {
            entry.group.kill();
        }
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        // A registry dropped without `close` (panic, cancelled session).
        for entry in self.lock().iter() {
            if entry.state() == State::Running {
                entry.group.kill();
            }
        }
        for supervisor in self
            .supervisors
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            supervisor.abort();
        }
    }
}

fn reader(
    entry: &Arc<Entry>,
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> JoinHandle<()> {
    let entry = Arc::clone(entry);
    tokio::spawn(async move { spawn::pump(stream, |bytes| entry.output().push(bytes)).await })
}

async fn supervise(
    entry: Arc<Entry>,
    mut child: tokio::process::Child,
    readers: Vec<JoinHandle<()>>,
    kill: oneshot::Receiver<()>,
    options: Options,
) {
    let (status, killed) = tokio::select! {
        status = child.wait() => (status.ok(), false),
        Ok(()) = kill => (spawn::stop(&mut child, entry.group, options.kill_grace).await, true),
    };
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(READ_GRACE, reader).await.is_err() {
            abort.abort();
        }
    }
    let exit_code = status.map_or(-1, spawn::exit_code);
    entry.state.send_replace(State::Finished {
        exit_code,
        at: (options.clock)(),
        killed,
    });
    if entry.notify
        && !killed
        && let Some(notify) = &options.notify
    {
        let _ = notify.send(Completion {
            process_id: entry.id.clone(),
            command: entry.command.clone(),
            exit_code,
        });
    }
}

fn describe(entry: &Entry) -> Value {
    match entry.state() {
        State::Running => json!({"status": "ok", "process_id": entry.id, "state": "running"}),
        State::Finished {
            exit_code, killed, ..
        } => json!({
            "status": "ok",
            "process_id": entry.id,
            "state": if killed { "killed" } else { "exited" },
            "exit_code": exit_code,
        }),
    }
}

async fn write(entry: &Entry, data: &str, submit: bool) -> Value {
    let mut stdin = entry.stdin.lock().await;
    let Some(pipe) = stdin.as_mut() else {
        return error(&format!("stdin of {} is closed", entry.id));
    };
    let mut bytes = data.as_bytes().to_vec();
    if submit {
        bytes.push(b'\n');
    }
    match pipe.write_all(&bytes).await.and(pipe.flush().await) {
        Ok(()) => json!({"status": "ok", "process_id": entry.id, "bytes_written": bytes.len()}),
        Err(err) => error(&format!("cannot write to {}: {err}", entry.id)),
    }
}

async fn close_stdin(entry: &Entry) -> Value {
    entry.stdin.lock().await.take();
    json!({"status": "ok", "process_id": entry.id, "stdin": "closed"})
}

fn error(message: &str) -> Value {
    json!({"status": "error", "error": message})
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn the_buffer_keeps_the_last_characters_and_utf8_split_across_chunks() {
        let mut output = Output::default();
        let bytes = "é".as_bytes();
        output.push(&bytes[..1]);
        output.push(&bytes[1..]);
        assert_eq!(output.text, "é");
        output.push("x".repeat(super::BUFFER_CHARS).as_bytes());
        assert_eq!(output.chars, super::BUFFER_CHARS);
        assert!(output.text.chars().all(|c| c == 'x'));
        assert_eq!(output.since(output.total - 3), "xxx");
    }
}
