//! The background process registry behind the `process` tool (spec:
//! execution-environments, Background process registry; Background
//! notifications are throttled; Subagent process handoff; Quiet-wait
//! auto-notify; Retained process receipts).
//!
//! Each session tracks at most [`MAX_PROCESSES`] processes; adding one more
//! prunes the oldest finished one. Every process keeps a rolling
//! [`BUFFER_CHARS`]-character output buffer and stays queryable until
//! [`EXPIRY`] after it *finished*; after that (and across compression and
//! restarts) its [`Receipt`] answers `poll` and `log`.
//! [`ProcessRegistry::close`] (session end) stops every process group, and
//! dropping the registry kills whatever is left, so a closed session leaves
//! no orphan behind.
//!
//! A process is a local child ([`ProcessRegistry::track`]) or a [`Job`]: a
//! command a `terminal-backend` plugin runs in one blocking `execute` call
//! ([`ProcessRegistry::track_job`]). A job's output arrives when the call
//! returns; stopping it signals its [`Group`], whose remote side asks the
//! plugin to `cancel`.
//!
//! A process reports through its sink: the session's notification channel
//! (behind the session-wide [`notice`] cap) and receipts. A handoff swaps
//! the sink for the parent session's, so the parent hears of it.

pub mod notice;
mod output;
pub mod receipts;
pub mod watch;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use pmt_core::secret::Redactor;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch as state_watch};
use tokio::task::JoinHandle;

pub use notice::{Completion, Notice, Progress};
pub use receipts::{Receipt, Receipts};

use self::notice::Limiter;
use self::output::Output;
use self::watch::{Verdict, Watch};
use super::shape;
use super::spawn::{self, Group, Spawned};

/// Processes tracked per session.
pub const MAX_PROCESSES: usize = 64;
/// Characters of output kept per process.
pub const BUFFER_CHARS: usize = 200_000;
/// How long a finished process stays queryable.
pub const EXPIRY: Duration = Duration::from_mins(30);
/// Processes one subagent may hand to its parent.
pub const MAX_HANDOFFS: u32 = 3;
/// The `process` tool's actions.
pub const ACTIONS: [&str; 9] = [
    "list", "poll", "log", "wait", "kill", "write", "submit", "close", "handoff",
];
/// What the second unchanged `poll`/`wait` of a running process says.
pub const QUIET_NOTE: &str = "No new output; you will be notified when it exits. Do other work or end your turn instead of polling.";

/// The id prefix of background processes.
const ID_PREFIX: &str = "proc_";
/// How long output readers may lag behind the process exit.
const READ_GRACE: Duration = Duration::from_secs(1);

/// The time source (replaceable in tests).
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// How a [`Job`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnd {
    /// Its exit code.
    pub exit_code: i32,
    /// Everything it printed (or why it has no result).
    pub output: String,
}

/// A background command that is not a local child process.
pub struct Job {
    /// Completes when the command ended.
    pub run: std::pin::Pin<Box<dyn std::future::Future<Output = JobEnd> + Send>>,
    /// Stops it: [`Group::terminate`] on kill and session end.
    pub group: Group,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

/// What a started background process asked to be told about.
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    /// Its command line.
    pub command: String,
    /// Notify when it exits (`notify_on_complete`, promotion).
    pub notify: bool,
    /// Lines to report while it runs.
    pub watch_patterns: Vec<String>,
    /// Progress notes this often ([`watch::heartbeat_interval`]).
    pub heartbeat: Option<Duration>,
}

impl Tracking {
    /// A process of `command` that notifies on exit when `notify`.
    #[must_use]
    pub fn new(command: &str, notify: bool) -> Self {
        Self {
            command: command.to_owned(),
            notify,
            ..Self::default()
        }
    }
}

/// Registry settings.
#[derive(Clone)]
pub struct Options {
    /// SIGTERM-to-SIGKILL wait (`tools.kill_grace_secs`).
    pub kill_grace: Duration,
    /// Characters returned per call (`tool_output.max_chars`).
    pub max_chars: crate::setting::Limit,
    /// Scrubs output before it is returned.
    pub redactor: Redactor,
    /// Where notifications go (none: the session cannot receive them).
    pub notify: Option<mpsc::UnboundedSender<Notice>>,
    /// The time source.
    pub clock: Clock,
    /// Two unchanged `poll`/`wait` calls switch completion notification on
    /// (`terminal.process_wait_auto_notify`, off where no later
    /// completion can arrive: cron, subagents, single queries).
    pub auto_notify: bool,
    /// Where ended processes leave their receipts.
    pub receipts: Option<Arc<dyn Receipts>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("kill_grace", &self.kill_grace)
            .field("max_chars", &(self.max_chars)())
            .field("auto_notify", &self.auto_notify)
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

/// A parent session that takes over processes of its subagent.
pub trait Adopter: Send + Sync {
    /// Takes `process` over (it reports to the adopter's session from now
    /// on).
    ///
    /// # Errors
    ///
    /// Why the adopter cannot take it (the parent ended, its registry is
    /// full).
    fn adopt(&self, process: HandedOff) -> Result<(), String>;
}

/// A running process on its way to another session's registry.
pub struct HandedOff(Arc<Entry>);

impl std::fmt::Debug for HandedOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HandedOff").field(&self.0.id).finish()
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

/// Where a process reports: notifications (behind the session cap) and
/// receipts.
#[derive(Clone)]
struct Sink {
    notify: Option<mpsc::UnboundedSender<Notice>>,
    limiter: Arc<Mutex<Limiter>>,
    receipts: Option<Arc<dyn Receipts>>,
    redactor: Redactor,
}

impl Sink {
    fn clean(&self, text: &str) -> String {
        self.redactor.redact_all(&shape::strip_ansi(text))
    }
}

struct Entry {
    id: String,
    command: String,
    started: Instant,
    started_ms: i64,
    pid: Option<u32>,
    group: Group,
    /// Notify on exit.
    notify: AtomicBool,
    /// Watch-pattern lines delivered so far (a goal parked on the process
    /// releases on a new one).
    matches: AtomicU32,
    /// A subagent handed it to this session.
    adopted: AtomicBool,
    sink: Mutex<Sink>,
    clock: Clock,
    output: Mutex<Output>,
    watch: Mutex<Watch>,
    /// `output.total` at the last `poll`/`wait`, while the calls on this
    /// process are only those.
    quiet: Mutex<Option<usize>>,
    state: state_watch::Sender<State>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    /// The supervisor and heartbeat tasks.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Entry {
    fn output(&self) -> MutexGuard<'_, Output> {
        lock(&self.output)
    }

    fn state(&self) -> State {
        *self.state.borrow()
    }

    fn sink(&self) -> Sink {
        lock(&self.sink).clone()
    }

    fn progress(&self, text: String) -> Progress {
        Progress {
            process_id: self.id.clone(),
            command: self.command.clone(),
            text,
        }
    }

    /// Sends `notice` unless the session cap holds it back.
    fn send(&self, notice: Notice) {
        let sink = self.sink();
        let Some(notify) = &sink.notify else {
            return;
        };
        if lock(&sink.limiter).admit(&notice, (self.clock)()) {
            let _ = notify.send(notice);
        }
    }

    /// New output: kept, and its complete lines checked against the watch.
    fn on_output(&self, bytes: &[u8]) {
        let text = self.output().push(bytes);
        if text.is_empty() {
            return;
        }
        let verdicts = lock(&self.watch).feed(&text, (self.clock)());
        for verdict in verdicts {
            let line = match verdict {
                Verdict::Deliver(line) => {
                    self.matches.fetch_add(1, Ordering::SeqCst);
                    line
                }
                Verdict::DeliverLast(line) => {
                    self.matches.fetch_add(1, Ordering::SeqCst);
                    self.notify.store(true, Ordering::SeqCst);
                    line
                }
                Verdict::Stop => {
                    self.notify.store(true, Ordering::SeqCst);
                    continue;
                }
            };
            let line = self.sink().clean(&line);
            self.send(Notice::Matched(self.progress(line)));
        }
    }

    fn reset_quiet(&self) {
        *lock(&self.quiet) = None;
    }
}

/// A running background process as [`ProcessRegistry::own_running`]
/// lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// Its process id (`proc_…`).
    pub id: String,
    /// Its OS pid, when local.
    pub pid: Option<u32>,
    /// Its command line.
    pub command: String,
    /// Watch-pattern lines delivered so far.
    pub matches: u32,
}

/// A process the registry found for a call.
enum Found {
    Live(Arc<Entry>),
    Retained(Receipt),
}

/// A session's background processes.
pub struct ProcessRegistry {
    options: Options,
    sink: Sink,
    entries: Mutex<Vec<Arc<Entry>>>,
    adopter: RwLock<Option<Arc<dyn Adopter>>>,
    handoffs: AtomicU32,
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
        let sink = Sink {
            notify: options.notify.clone(),
            limiter: Arc::default(),
            receipts: options.receipts.clone(),
            redactor: options.redactor.clone(),
        };
        Self {
            options,
            sink,
            entries: Mutex::new(Vec::new()),
            adopter: RwLock::new(None),
            handoffs: AtomicU32::new(0),
        }
    }

    /// Makes this a subagent's registry: `handoff` gives processes to
    /// `parent`.
    pub fn set_adopter(&self, parent: Arc<dyn Adopter>) {
        *self.adopter.write().unwrap_or_else(PoisonError::into_inner) = Some(parent);
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Arc<Entry>>> {
        lock(&self.entries)
    }

    fn now(&self) -> Instant {
        (self.options.clock)()
    }

    /// Drops processes that finished more than [`EXPIRY`] ago (their
    /// receipts stay).
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

    fn entry(
        &self,
        tracking: &Tracking,
        process: (Option<u32>, Group, Option<ChildStdin>),
    ) -> (Arc<Entry>, oneshot::Receiver<()>) {
        let (pid, group, stdin) = process;
        let (kill_tx, kill_rx) = oneshot::channel();
        let entry = Arc::new(Entry {
            id: format!(
                "{ID_PREFIX}{}",
                ulid::Ulid::generate().to_string().to_lowercase()
            ),
            command: tracking.command.clone(),
            started: self.now(),
            started_ms: receipts::now_ms(),
            pid,
            group,
            notify: AtomicBool::new(tracking.notify),
            matches: AtomicU32::new(0),
            adopted: AtomicBool::new(false),
            sink: Mutex::new(self.sink.clone()),
            clock: Arc::clone(&self.options.clock),
            output: Mutex::default(),
            watch: Mutex::new(Watch::new(tracking.watch_patterns.clone())),
            quiet: Mutex::new(None),
            state: state_watch::Sender::new(State::Running),
            stdin: tokio::sync::Mutex::new(stdin),
            kill: Mutex::new(Some(kill_tx)),
            tasks: Mutex::new(Vec::new()),
        });
        (entry, kill_rx)
    }

    fn register(
        &self,
        entry: Arc<Entry>,
        supervisor: JoinHandle<()>,
        tracking: &Tracking,
    ) -> String {
        let id = entry.id.clone();
        let mut tasks = vec![supervisor];
        if let Some(every) = tracking.heartbeat {
            tasks.push(pmt_core::spawn::spawn(heartbeat(
                Arc::clone(&entry),
                every,
                (self.options.max_chars)(),
            )));
        }
        lock(&entry.tasks).extend(tasks);
        self.lock().push(entry);
        id
    }

    /// Tracks a started process and returns its id. Call
    /// [`ProcessRegistry::reserve`] before starting it.
    pub fn track(&self, spawned: Spawned, tracking: &Tracking) -> String {
        let Spawned { mut child, group } = spawned;
        let process = (child.id(), group, child.stdin.take());
        let (entry, kill_rx) = self.entry(tracking, process);
        let readers: Vec<JoinHandle<()>> = [
            child.stdout.take().map(|r| reader(&entry, r)),
            child.stderr.take().map(|r| reader(&entry, r)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let supervisor = pmt_core::spawn::spawn(supervise(
            Arc::clone(&entry),
            child,
            readers,
            kill_rx,
            self.options.kill_grace,
        ));
        self.register(entry, supervisor, tracking)
    }

    /// Tracks a started [`Job`] and returns its id (it has no pid and no
    /// stdin). Call [`ProcessRegistry::reserve`] before starting it.
    pub fn track_job(&self, job: Job, tracking: &Tracking) -> String {
        let Job { run, group } = job;
        let (entry, kill_rx) = self.entry(tracking, (None, group, None));
        let supervisor = pmt_core::spawn::spawn(supervise_job(Arc::clone(&entry), run, kill_rx));
        self.register(entry, supervisor, tracking)
    }

    /// Takes over a process handed off by a subagent: it reports to this
    /// session, which is told when it exits.
    ///
    /// # Errors
    ///
    /// This registry has no room.
    pub fn adopt(&self, process: HandedOff) -> Result<(), String> {
        self.reserve()?;
        let entry = process.0;
        *lock(&entry.sink) = self.sink.clone();
        entry.notify.store(true, Ordering::SeqCst);
        entry.adopted.store(true, Ordering::SeqCst);
        entry.reset_quiet();
        self.lock().push(entry);
        Ok(())
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
            Ok(Found::Live(entry)) => entry,
            Ok(Found::Retained(receipt)) => return self.retained(&receipt, action),
            Err(message) => return error(&message),
        };
        if !matches!(action, "poll" | "wait") {
            entry.reset_quiet();
        }
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
            _ => self.handoff(&entry),
        }
    }

    fn find(&self, prefix: &str) -> Result<Found, String> {
        let bare = prefix.strip_prefix(ID_PREFIX).unwrap_or(prefix);
        let live: Vec<Arc<Entry>> = self
            .lock()
            .iter()
            .filter(|e| e.id[ID_PREFIX.len()..].starts_with(bare))
            .cloned()
            .collect();
        match live.as_slice() {
            [one] => return Ok(Found::Live(Arc::clone(one))),
            [] => {}
            many => return Err(ambiguous(prefix, many.iter().map(|e| e.id.as_str()))),
        }
        let retained = self
            .sink
            .receipts
            .as_ref()
            .map(|r| r.matching(prefix))
            .unwrap_or_default();
        match retained.as_slice() {
            [one] => Ok(Found::Retained(one.clone())),
            [] => Err(format!(
                "unknown process id '{prefix}'; list them with action=list"
            )),
            many => Err(ambiguous(
                prefix,
                many.iter().map(|r| r.process_id.as_str()),
            )),
        }
    }

    fn list(&self) -> Value {
        let now = self.now();
        let live = self.lock().clone();
        let mut processes: Vec<Value> = live
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
        let retained = self
            .sink
            .receipts
            .as_ref()
            .map(|r| r.all())
            .unwrap_or_default();
        processes.extend(
            retained
                .iter()
                .filter(|r| !live.iter().any(|e| e.id == r.process_id))
                .map(|r| {
                    let mut item = describe_receipt(r);
                    item["command"] = json!(r.command);
                    item
                }),
        );
        json!({"status": "ok", "processes": processes})
    }

    fn shaped(&self, text: &str) -> String {
        // Background output is not a foreground result: no hooks run.
        let clean = self.sink.clean(text);
        shape::truncate(&clean, (self.options.max_chars)()).0
    }

    /// What `poll`/`wait` return: the output since the last of them.
    fn polled(&self, entry: &Entry, note: Option<String>) -> Value {
        let mut output = entry.output();
        let total = output.total;
        let new = self.shaped(output.since(output.polled));
        output.polled = total;
        drop(output);
        let mut result = describe(entry);
        result["output"] = json!(new);
        if let Some(note) = note {
            result["note"] = json!(note);
        }
        self.quiet_wait(entry, total, &mut result);
        result
    }

    fn poll(&self, entry: &Entry) -> Value {
        self.polled(entry, None)
    }

    /// Quiet-wait auto-notify: the second `poll`/`wait` in a row that finds
    /// a running process silent switches its completion notification on.
    fn quiet_wait(&self, entry: &Entry, total: usize, result: &mut Value) {
        let mut quiet = lock(&entry.quiet);
        if entry.state() != State::Running {
            *quiet = None;
            return;
        }
        let unchanged = *quiet == Some(total);
        *quiet = Some(total);
        drop(quiet);
        let receivable = entry.sink().notify.is_some();
        if unchanged && self.options.auto_notify && receivable {
            entry.notify.store(true, Ordering::SeqCst);
            let note = match result.get("note").and_then(Value::as_str) {
                Some(note) => format!("{note}. {QUIET_NOTE}"),
                None => QUIET_NOTE.to_owned(),
            };
            result["note"] = json!(note);
        }
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
        let note = finished
            .is_err()
            .then(|| format!("still running after {}s", limit.as_secs()));
        self.polled(entry, note)
    }

    async fn kill(&self, entry: &Entry) -> Value {
        let sender = lock(&entry.kill).take();
        if let Some(sender) = sender {
            let _ = sender.send(());
            let mut state = entry.state.subscribe();
            let limit = self.options.kill_grace + Duration::from_secs(5);
            let _ = tokio::time::timeout(limit, state.wait_for(|s| *s != State::Running)).await;
        }
        describe(entry)
    }

    /// `handoff`: a subagent gives a running process to its parent.
    fn handoff(&self, entry: &Arc<Entry>) -> Value {
        let adopter = self
            .adopter
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(adopter) = adopter else {
            return error("handoff is available only to subagent sessions");
        };
        if entry.state() != State::Running {
            return error(&format!("{} is not running; nothing to hand off", entry.id));
        }
        if self.handoffs.load(Ordering::SeqCst) >= MAX_HANDOFFS {
            return error(&format!(
                "handoff cap reached ({MAX_HANDOFFS} per subagent); wait on or kill the rest yourself."
            ));
        }
        if let Err(why) = adopter.adopt(HandedOff(Arc::clone(entry))) {
            return error(&format!("cannot hand {} off: {why}", entry.id));
        }
        self.handoffs.fetch_add(1, Ordering::SeqCst);
        self.lock().retain(|e| !Arc::ptr_eq(e, entry));
        json!({"status": "ok", "process_id": entry.id, "state": "running", "handed_off": true})
    }

    /// `poll`/`log`/`wait` of a process only its receipt remembers.
    fn retained(&self, receipt: &Receipt, action: &str) -> Value {
        if !matches!(action, "poll" | "log" | "wait") {
            return error(&format!(
                "{} has ended; only poll, log and wait read its receipt",
                receipt.process_id
            ));
        }
        let mut result = describe_receipt(receipt);
        result["output"] = json!(shape::truncate(&receipt.output, (self.options.max_chars)()).0);
        result
    }

    /// OS pids of processes still running (the session may kill them even
    /// where the self-termination guard applies).
    #[must_use]
    pub fn running_pids(&self) -> Vec<u32> {
        self.lock()
            .iter()
            .filter(|e| e.state() == State::Running)
            .filter_map(|e| e.pid)
            .collect()
    }

    /// Running processes the session started itself (not ones its
    /// subagents handed over), for the goal judge (spec: session-
    /// automation, Goal wait barriers).
    #[must_use]
    pub fn own_running(&self) -> Vec<Running> {
        self.lock()
            .iter()
            .filter(|e| e.state() == State::Running && !e.adopted.load(Ordering::SeqCst))
            .map(|e| Running {
                id: e.id.clone(),
                pid: e.pid,
                command: e.command.clone(),
                matches: e.matches.load(Ordering::SeqCst),
            })
            .collect()
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
        let entries = self.lock().clone();
        for entry in &entries {
            if let Some(sender) = lock(&entry.kill).take() {
                let _ = sender.send(());
            }
        }
        let limit = self.options.kill_grace + Duration::from_secs(5);
        for entry in &entries {
            let tasks = std::mem::take(&mut *lock(&entry.tasks));
            for task in tasks {
                let abort = task.abort_handle();
                if tokio::time::timeout(limit, task).await.is_err() {
                    abort.abort();
                }
            }
        }
        // Stragglers that left the leader's group alive.
        for entry in &entries {
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
            for task in lock(&entry.tasks).drain(..) {
                task.abort();
            }
        }
    }
}

fn reader(
    entry: &Arc<Entry>,
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> JoinHandle<()> {
    let entry = Arc::clone(entry);
    pmt_core::spawn::spawn(async move { spawn::pump(stream, |bytes| entry.on_output(bytes)).await })
}

async fn supervise(
    entry: Arc<Entry>,
    mut child: tokio::process::Child,
    readers: Vec<JoinHandle<()>>,
    kill: oneshot::Receiver<()>,
    kill_grace: Duration,
) {
    let (status, killed) = tokio::select! {
        status = child.wait() => (status.ok(), false),
        Ok(()) = kill => (spawn::stop(&mut child, &entry.group, kill_grace).await, true),
    };
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(READ_GRACE, reader).await.is_err() {
            abort.abort();
        }
    }
    finish(&entry, status.map_or(-1, spawn::exit_code), killed);
}

async fn supervise_job(
    entry: Arc<Entry>,
    run: std::pin::Pin<Box<dyn std::future::Future<Output = JobEnd> + Send>>,
    kill: oneshot::Receiver<()>,
) {
    let (exit_code, killed) = tokio::select! {
        end = run => {
            entry.on_output(end.output.as_bytes());
            (end.exit_code, false)
        }
        Ok(()) = kill => {
            // The call's result, if it ever comes, is dropped.
            entry.group.terminate();
            (-1, true)
        }
    };
    finish(&entry, exit_code, killed);
}

/// Sends a heartbeat every `every` with the output since the last one,
/// until the process ends.
async fn heartbeat(entry: Arc<Entry>, every: Duration, max_chars: usize) {
    let mut state = entry.state.subscribe();
    loop {
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            _ = state.wait_for(|s| *s != State::Running) => return,
        }
        let text = {
            let mut output = entry.output();
            let text = output.since(output.beat).to_owned();
            output.beat = output.total;
            text
        };
        let clean = entry.sink().clean(&text);
        let text = shape::truncate(&clean, max_chars).0;
        entry.send(Notice::Heartbeat(entry.progress(text)));
    }
}

/// Records the end of a process, keeps its receipt and sends its
/// completion notification.
fn finish(entry: &Entry, exit_code: i32, killed: bool) {
    entry.state.send_replace(State::Finished {
        exit_code,
        at: (entry.clock)(),
        killed,
    });
    let sink = entry.sink();
    if let Some(receipts) = &sink.receipts {
        let output = sink.clean(&entry.output().text);
        receipts.keep(Receipt {
            process_id: entry.id.clone(),
            command: entry.command.clone(),
            exit_code,
            started_at_ms: entry.started_ms,
            ended_at_ms: receipts::now_ms(),
            output,
        });
    }
    if entry.notify.load(Ordering::SeqCst) && !killed {
        entry.send(Notice::Exited(Completion {
            process_id: entry.id.clone(),
            command: entry.command.clone(),
            exit_code,
        }));
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

fn describe_receipt(receipt: &Receipt) -> Value {
    json!({
        "status": "ok",
        "process_id": receipt.process_id,
        "state": "exited",
        "exit_code": receipt.exit_code,
        "retained": true,
    })
}

fn ambiguous<'a>(prefix: &str, ids: impl Iterator<Item = &'a str>) -> String {
    format!(
        "'{prefix}' matches several processes: {}",
        ids.collect::<Vec<_>>().join(", ")
    )
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
