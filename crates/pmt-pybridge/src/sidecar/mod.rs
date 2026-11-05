//! One sidecar process (spec: python-bridge, Sidecar wire protocol; Call
//! timeouts and stall detection; Crash isolation and restart policy;
//! Resource limits; Sidecar lifecycle).
//!
//! The process leads its own process group so its whole tree can be
//! terminated. Three threads serve it: one reads frames from its stdout
//! (answers to our calls, and `host.call` requests it makes, each served
//! on its own thread), one copies its stderr into the plugin log, and one
//! samples the tree against the resource limits. Calls are blocking with
//! their own timeout, so the bridge works from any thread, inside an
//! async runtime or not. When the process ends, every call in flight
//! fails with `bridge_crashed: plugin '<id>' exited (<status>)`, the
//! status naming the limit or protocol violation that made the host kill
//! it, and the `on_exit` callback reports it.

mod process;

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use crate::limits::{self, Limits};
use crate::log::Log;
use crate::wire::{self, FrameError, HOST_CALL, PROTOCOL_VERSION, UNKNOWN_METHOD};

pub use process::{exit_text, terminate_tree};

/// How long `bridge.hello` may take.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
/// `bridge.shutdown`, then terminate, then kill (spec: 5 + 2 seconds).
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// From terminate to the forced kill.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// A `host.call` the host refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFault {
    /// Machine-readable code (`capability_denied`, `unavailable`...).
    pub code: String,
    /// What happened.
    pub message: String,
}

impl HostFault {
    /// A fault.
    #[must_use]
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Serves the requests a sidecar makes (`host.call`).
pub trait HostHandler: Send + Sync {
    /// Runs `method` with `args` for the call described by `context`.
    ///
    /// # Errors
    ///
    /// The fault the sidecar sees as a `HostError`.
    fn handle(&self, method: &str, args: Value, context: &Value) -> Result<Value, HostFault>;
}

/// Why a call into a sidecar failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No answer within the bound.
    Timeout(Duration),
    /// The process ended (`bridge_crashed: ...`).
    Crashed(String),
    /// The plugin answered with an error.
    Remote {
        /// `data.code` (`plugin_error`, `unsupported_api`, `frame_too_large`...).
        code: String,
        /// Its message.
        message: String,
    },
}

impl CallError {
    /// The text a tool result or log shows.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Timeout(bound) => format!("bridge call timed out after {} ms", bound.as_millis()),
            Self::Crashed(text) => text.clone(),
            Self::Remote { message, .. } => message.clone(),
        }
    }

    /// As a `bridge.call` error.
    #[must_use]
    pub fn into_error(self) -> Error {
        Error::new(&codes::BRIDGE_CALL, self.message())
    }
}

type Reply = Result<Value, CallError>;

/// Called once when the process has ended, with its status text.
pub type OnExit = Box<dyn Fn(String) + Send + Sync>;

/// What starting a sidecar needs.
pub struct Spawn {
    /// The confined command (`python -I -B bridge_main.py`).
    pub command: Command,
    /// The plugin id.
    pub plugin: String,
    /// Its log.
    pub log: Arc<Log>,
    /// Its resource limits.
    pub limits: Limits,
    /// Serves its `host.call` requests.
    pub host: Arc<dyn HostHandler>,
    /// Told when the process ends.
    pub on_exit: OnExit,
}

struct Inner {
    plugin: String,
    pid: u32,
    started: Instant,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Reply>>>,
    next: AtomicU64,
    kill_reason: Mutex<Option<String>>,
    ended: Mutex<Option<String>>,
    stopping: AtomicBool,
    log: Arc<Log>,
    memory: AtomicU64,
    limits: Limits,
    /// The descendants the last sample saw.
    members: Mutex<Vec<limits::Member>>,
}

/// A running sidecar (cheap to clone).
#[derive(Clone)]
pub struct Sidecar {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Sidecar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sidecar")
            .field("plugin", &self.inner.plugin)
            .field("pid", &self.inner.pid)
            .finish_non_exhaustive()
    }
}

impl Sidecar {
    /// Starts the process and completes the `bridge.hello` handshake.
    ///
    /// # Errors
    ///
    /// `bridge.call` when the process cannot start or does not answer;
    /// `bridge.protocol` (`bridge protocol mismatch (host 1, sidecar N)`)
    /// when it speaks another protocol version (it is stopped).
    pub fn start(spawn: Spawn, compat_level: &str) -> Result<Self, Error> {
        let Spawn {
            mut command,
            plugin,
            log,
            limits,
            host,
            on_exit,
        } = spawn;
        process::new_group(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| {
            Error::new(
                &codes::BRIDGE_CALL,
                format!("plugin '{plugin}': the sidecar did not start: {e}"),
            )
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let inner = Arc::new(Inner {
            pid: child.id(),
            plugin: plugin.clone(),
            started: Instant::now(),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            kill_reason: Mutex::new(None),
            ended: Mutex::new(None),
            stopping: AtomicBool::new(false),
            log: Arc::clone(&log),
            memory: AtomicU64::new(0),
            limits,
            members: Mutex::new(Vec::new()),
        });
        log.note(&format!("started (pid {})", inner.pid));
        if let Some(stderr) = stderr {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log.line(&line);
                }
            });
        }
        if let Some(stdout) = stdout {
            let reader = Arc::clone(&inner);
            std::thread::spawn(move || read_loop(&reader, stdout, &host, &on_exit));
        }
        let monitor = Arc::clone(&inner);
        std::thread::spawn(move || watch(&monitor));
        let sidecar = Self { inner };
        sidecar.hello(compat_level)?;
        Ok(sidecar)
    }

    fn hello(&self, compat_level: &str) -> Result<(), Error> {
        let params = json!({
            "protocol_version": PROTOCOL_VERSION,
            "compat_level": compat_level,
            "plugin_id": self.inner.plugin,
        });
        let answer = self
            .call("bridge.hello", &params, HELLO_TIMEOUT)
            .map_err(CallError::into_error)?;
        let theirs = answer["protocol_version"].as_u64().unwrap_or(0);
        if theirs != PROTOCOL_VERSION {
            let message =
                format!("bridge protocol mismatch (host {PROTOCOL_VERSION}, sidecar {theirs})");
            self.kill(&message);
            return Err(Error::new(&codes::BRIDGE_PROTOCOL, message));
        }
        Ok(())
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Time since start.
    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    /// Resident bytes of the tree at the last sample.
    #[must_use]
    pub fn memory(&self) -> u64 {
        self.inner.memory.load(Ordering::Relaxed)
    }

    /// The exit status text once the process has ended.
    #[must_use]
    pub fn ended(&self) -> Option<String> {
        self.inner
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Calls `method` and waits for its answer up to `timeout`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub fn call(&self, method: &str, params: &Value, timeout: Duration) -> Reply {
        if let Some(status) = self.ended() {
            return Err(self.crashed(&status));
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        let message = wire::request(&json!(id), method, params);
        if let Err(err) = self.write(&message) {
            self.forget(id);
            return Err(match err.kind() {
                std::io::ErrorKind::InvalidInput => CallError::Remote {
                    code: wire::FRAME_TOO_LARGE.to_owned(),
                    message: err.to_string(),
                },
                _ => self.crashed(
                    &self
                        .ended()
                        .unwrap_or_else(|| "its input closed".to_owned()),
                ),
            });
        }
        match receiver.recv_timeout(timeout) {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget(id);
                Err(CallError::Timeout(timeout))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(self.crashed(&self.ended().unwrap_or_default()))
            }
        }
    }

    fn forget(&self, id: u64) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    fn write(&self, message: &Value) -> std::io::Result<()> {
        let mut stdin = self
            .inner
            .stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match stdin.as_mut() {
            Some(pipe) => wire::write_frame(pipe, message),
            None => Err(std::io::Error::other("closed")),
        }
    }

    fn crashed(&self, status: &str) -> CallError {
        CallError::Crashed(crash_text(&self.inner.plugin, status))
    }

    /// Answers `bridge.ping` within `timeout`.
    #[must_use]
    pub fn ping(&self, timeout: Duration) -> bool {
        self.call("bridge.ping", &json!({}), timeout).is_ok()
    }

    /// Kills the process tree; in-flight calls fail with `bridge_crashed`
    /// naming `reason`.
    pub fn kill(&self, reason: &str) {
        self.inner
            .kill_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| reason.to_owned());
        let mut system = sysinfo::System::new();
        if let Some(tree) = limits::sample(&mut system, self.inner.pid) {
            *self.inner.lock_members() = tree.descendants;
        }
        terminate_tree(self.inner.pid, true);
        limits::kill_members(&mut system, &self.inner.lock_members());
        let _ = self
            .inner
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill();
    }

    /// Stops it on purpose: `bridge.shutdown` (the plugin's `on_unload`
    /// callbacks run), up to 5 seconds to exit, then terminate the tree
    /// and kill it 2 seconds later.
    pub fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::Relaxed);
        if self.ended().is_some() {
            return;
        }
        let started = Instant::now();
        let _ = self.call("bridge.shutdown", &json!({}), SHUTDOWN_GRACE);
        // Children the plugin started may outlive its own exit.
        let left = SHUTDOWN_GRACE.saturating_sub(started.elapsed());
        if !self.wait_end(left) {
            terminate_tree(self.inner.pid, false);
            if self.wait_end(KILL_GRACE) {
                terminate_tree(self.inner.pid, true);
                return;
            }
        }
        self.kill("stopped");
    }

    /// Whether the host stopped it on purpose.
    #[must_use]
    pub fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::Relaxed)
    }

    fn wait_end(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while Instant::now() < deadline {
            if self.ended().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.ended().is_some()
    }
}

/// `bridge_crashed: plugin '<id>' exited (<status>)`.
#[must_use]
pub fn crash_text(plugin: &str, status: &str) -> String {
    format!("bridge_crashed: plugin '{plugin}' exited ({status})")
}

fn read_loop(
    inner: &Arc<Inner>,
    mut stdout: ChildStdout,
    host: &Arc<dyn HostHandler>,
    on_exit: &OnExit,
) {
    loop {
        match wire::read_frame(&mut stdout) {
            Ok(Some(message)) => dispatch(inner, message, host),
            Ok(None) | Err(FrameError::Io(_)) => break,
            Err(err @ FrameError::TooLarge(_)) => {
                let text = err.to_string();
                fail_all(
                    inner,
                    &CallError::Remote {
                        code: wire::FRAME_TOO_LARGE.to_owned(),
                        message: text.clone(),
                    },
                );
                Sidecar {
                    inner: Arc::clone(inner),
                }
                .kill(&text);
                break;
            }
            Err(err @ FrameError::Json(_)) => {
                Sidecar {
                    inner: Arc::clone(inner),
                }
                .kill(&format!("protocol violation: {err}"));
                break;
            }
        }
    }
    let status = {
        let mut child = inner.child.lock().unwrap_or_else(PoisonError::into_inner);
        match child.wait() {
            Ok(s) if s.code() == Some(limits::MEMORY_EXIT) => inner.limits.memory_exceeded(),
            Ok(s) => exit_text(&s),
            Err(e) => e.to_string(),
        }
    };
    // Children it left behind die with it, in its group or not.
    terminate_tree(inner.pid, true);
    limits::kill_members(&mut sysinfo::System::new(), &inner.lock_members());
    let reason = inner
        .kill_reason
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let status = reason.unwrap_or(status);
    *inner.ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(status.clone());
    inner
        .stdin
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    fail_all(
        inner,
        &CallError::Crashed(crash_text(&inner.plugin, &status)),
    );
    inner.log.note(&format!("exited ({status})"));
    on_exit(status);
}

fn fail_all(inner: &Inner, error: &CallError) {
    let pending: Vec<_> = inner
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
        .collect();
    for (_, sender) in pending {
        let _ = sender.send(Err(error.clone()));
    }
}

fn dispatch(inner: &Arc<Inner>, message: Value, host: &Arc<dyn HostHandler>) {
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if method != HOST_CALL {
            let reply = wire::error(
                &id,
                UNKNOWN_METHOD,
                &format!("unknown method '{method}'"),
                "unknown_method",
            );
            let _ = Sidecar {
                inner: Arc::clone(inner),
            }
            .write(&reply);
            return;
        }
        let (inner, host) = (Arc::clone(inner), Arc::clone(host));
        std::thread::spawn(move || {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let name = params["method"].as_str().unwrap_or("").to_owned();
            let reply = match host.handle(&name, params["args"].clone(), &params["context"]) {
                Ok(result) => wire::result(&id, &result),
                Err(fault) => wire::error(&id, -32000, &fault.message, &fault.code),
            };
            let _ = Sidecar { inner }.write(&reply);
        });
        return;
    }
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let sender = inner
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
    let Some(sender) = sender else { return };
    let reply = match message.get("error") {
        Some(error) => Err(CallError::Remote {
            code: error["data"]["code"]
                .as_str()
                .unwrap_or("plugin_error")
                .to_owned(),
            message: error["message"].as_str().unwrap_or("").to_owned(),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = sender.send(reply);
}

impl Inner {
    fn lock_members(&self) -> std::sync::MutexGuard<'_, Vec<limits::Member>> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Samples the tree until the process ends; kills it over a limit.
fn watch(inner: &Arc<Inner>) {
    let limits = inner.limits;
    let mut system = sysinfo::System::new();
    loop {
        if inner
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        if let Some(tree) = limits::sample(&mut system, inner.pid) {
            inner.memory.store(tree.usage.memory, Ordering::Relaxed);
            let usage = tree.usage;
            *inner.lock_members() = tree.descendants;
            if let Some(reason) = limits.exceeded(usage) {
                inner.log.note(&reason);
                Sidecar {
                    inner: Arc::clone(inner),
                }
                .kill(&reason);
                return;
            }
        }
        std::thread::sleep(limits::SAMPLE_EVERY);
    }
}
