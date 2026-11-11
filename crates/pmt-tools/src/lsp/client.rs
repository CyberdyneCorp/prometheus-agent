//! A language server process and its JSON-RPC session (spec:
//! lsp-diagnostics, Diagnostics introduced by an edit).
//!
//! One reader task owns the server's stdout: it routes responses to their
//! requests, answers the server's own requests (`workspace/configuration`
//! from the settings the client was started with, everything else with
//! `null`) and records every `publishDiagnostics` with a sequence number,
//! so a check can wait for the verdict on the version it sent.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use super::protocol::{self, Diagnostic};

/// The diagnostics a server last published for one document.
#[derive(Debug, Clone)]
struct Published {
    version: Option<i64>,
    seq: u64,
    diagnostics: Vec<Diagnostic>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;
type Store = Arc<Mutex<HashMap<String, Published>>>;
type Stdin = Arc<tokio::sync::Mutex<ChildStdin>>;

/// A running server.
pub struct Client {
    stdin: Stdin,
    child: Mutex<Child>,
    pid: Option<u32>,
    next_id: AtomicI64,
    pending: Pending,
    published: Store,
    seq: watch::Receiver<u64>,
    exited: Arc<AtomicBool>,
    capabilities: Value,
}

/// How a server is started.
pub struct Start<'a> {
    /// The server id (its stderr is logged at DEBUG under it).
    pub id: &'a str,
    /// The confined command (stdio is set here).
    pub command: Command,
    /// Its workspace root.
    pub root: &'a Path,
    /// `initializationOptions`.
    pub initialization_options: Value,
    /// What `workspace/configuration` answers, by section.
    pub settings: Value,
    /// Bound on starting and initializing.
    pub timeout: Duration,
}

impl Client {
    /// Spawns the server and runs the `initialize` handshake.
    ///
    /// # Errors
    ///
    /// The process cannot start, or does not initialize in time.
    pub async fn start(start: Start<'_>) -> Result<Self, String> {
        let Start {
            id,
            mut command,
            root,
            initialization_options,
            settings,
            timeout,
        } = start;
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| format!("cannot start: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            pmt_core::spawn::spawn(log_stderr(id.to_owned(), stderr));
        }
        let (seq_tx, seq) = watch::channel(0);
        let pending = Pending::default();
        let published = Store::default();
        let exited = Arc::new(AtomicBool::new(false));
        let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let client = Self {
            stdin: Arc::clone(&stdin),
            pid: child.id(),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: Arc::clone(&pending),
            published: Arc::clone(&published),
            seq,
            exited: Arc::clone(&exited),
            capabilities: Value::Null,
        };
        let reader = Reader {
            stdin,
            pending,
            published,
            seq: seq_tx,
            exited,
            settings: settings.clone(),
            folders: json!([{"uri": protocol::uri(root), "name": folder_name(root)}]),
        };
        pmt_core::spawn::spawn(reader.run(BufReader::new(stdout)));
        client
            .initialize(root, initialization_options, settings, timeout)
            .await
    }

    async fn initialize(
        mut self,
        root: &Path,
        options: Value,
        settings: Value,
        timeout: Duration,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let params = json!({
            // No parent pid: servers built on vscode-languageserver probe
            // it with kill(pid, 0) and exit when the probe fails, which it
            // always does under Seatbelt (signals stay inside the sandbox).
            // A server still ends with us: its stdin closes and the child
            // is killed on drop.
            "processId": Value::Null,
            "rootUri": protocol::uri(root),
            "rootPath": root.display().to_string(),
            "workspaceFolders": [{"uri": protocol::uri(root), "name": folder_name(root)}],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {"versionSupport": true},
                    "synchronization": {"didSave": false, "dynamicRegistration": false},
                },
                "workspace": {"configuration": true, "workspaceFolders": true},
                "window": {"workDoneProgress": false},
            },
            "initializationOptions": options,
        });
        let reply = self.request("initialize", params, deadline).await?;
        self.capabilities = reply.get("capabilities").cloned().unwrap_or(Value::Null);
        self.notify("initialized", json!({})).await?;
        // pyright (and others) wait for a configuration before analysing;
        // the settings themselves are answered to `workspace/configuration`.
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": settings}),
        )
        .await?;
        Ok(self)
    }

    /// The server's process id.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether the server process is still serving.
    #[must_use]
    pub fn alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }

    /// Whether the server answers `workspace/diagnostic`.
    #[must_use]
    pub fn has_workspace_diagnostics(&self) -> bool {
        self.capabilities
            .pointer("/diagnosticProvider/workspaceDiagnostics")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Sends a request and waits for its result until `deadline`.
    ///
    /// # Errors
    ///
    /// The server failed it, exited, or the deadline passed.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let reply = tokio::time::timeout_at(deadline, rx).await;
        lock(&self.pending).remove(&id);
        match reply {
            Err(_) => Err(format!("{method} timed out")),
            Ok(Err(_)) => Err("the server exited".to_owned()),
            Ok(Ok(reply)) => match reply.get("error") {
                Some(error) => Err(format!("{method} failed: {error}")),
                None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
            },
        }
    }

    /// Sends a notification.
    ///
    /// # Errors
    ///
    /// The server's stdin is closed.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn send(&self, message: &Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        protocol::write_message(&mut *stdin, message)
            .await
            .map_err(|e| format!("the server stopped reading: {e}"))
    }

    /// The current publish sequence number: a publish after it is newer
    /// than anything sent before.
    #[must_use]
    pub fn seq(&self) -> u64 {
        *self.seq.borrow()
    }

    /// The diagnostics published for `uri` at `version` (or, from a server
    /// that sends no versions, the first publish after `after_seq`).
    pub async fn diagnostics(
        &self,
        uri: &str,
        version: i64,
        after_seq: u64,
        deadline: Instant,
    ) -> Option<Vec<Diagnostic>> {
        let mut seq = self.seq.clone();
        loop {
            if let Some(found) = lock(&self.published).get(uri).filter(|p| match p.version {
                Some(v) => v == version,
                None => p.seq > after_seq,
            }) {
                return Some(found.diagnostics.clone());
            }
            if !self.alive() {
                return None;
            }
            match tokio::time::timeout_at(deadline, seq.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    /// Every document's last published diagnostics.
    #[must_use]
    pub fn published(&self) -> HashMap<String, Vec<Diagnostic>> {
        lock(&self.published)
            .iter()
            .map(|(uri, p)| (uri.clone(), p.diagnostics.clone()))
            .collect()
    }

    /// `workspace/diagnostic` (full reports by URI).
    ///
    /// # Errors
    ///
    /// The request failed.
    pub async fn workspace_diagnostics(
        &self,
        deadline: Instant,
    ) -> Result<HashMap<String, Vec<Diagnostic>>, String> {
        let reply = self
            .request(
                "workspace/diagnostic",
                json!({"previousResultIds": []}),
                deadline,
            )
            .await?;
        Ok(reply
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let uri = item.get("uri")?.as_str()?.to_owned();
                Some((uri, protocol::parse_list(item)))
            })
            .collect())
    }

    /// Asks the server to exit, then ends the process.
    pub async fn shutdown(&self) {
        if self.alive() {
            let deadline = Instant::now() + Duration::from_secs(1);
            let _ = self.request("shutdown", Value::Null, deadline).await;
            let _ = self.notify("exit", Value::Null).await;
        }
        let mut child = lock(&self.child);
        let _ = child.start_kill();
    }
}

/// Logs what a server writes to stderr (at DEBUG: servers are chatty, and
/// a server that dies early says why there).
async fn log_stderr(id: String, stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(server = %id, "stderr: {line}");
    }
}

fn folder_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The task reading the server's stdout.
struct Reader {
    stdin: Stdin,
    pending: Pending,
    published: Store,
    seq: watch::Sender<u64>,
    exited: Arc<AtomicBool>,
    settings: Value,
    folders: Value,
}

impl Reader {
    async fn run(self, mut stdout: BufReader<tokio::process::ChildStdout>) {
        while let Ok(Some(message)) = protocol::read_message(&mut stdout).await {
            if let Some(reply) = self.handle(message) {
                let mut stdin = self.stdin.lock().await;
                if protocol::write_message(&mut *stdin, &reply).await.is_err() {
                    break;
                }
            }
        }
        self.exited.store(true, Ordering::SeqCst);
        lock(&self.pending).clear();
        self.seq.send_modify(|s| *s += 1);
    }

    /// Records a notification or response; returns the answer to a server
    /// request.
    fn handle(&self, message: Value) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str);
        tracing::trace!(method = ?method, id = ?message.get("id"), "from the language server");
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                if let Some(tx) = id.as_i64().and_then(|id| lock(&self.pending).remove(&id)) {
                    let _ = tx.send(message);
                }
                None
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                self.record(message.get("params").unwrap_or(&Value::Null));
                None
            }
            (Some(method), Some(id)) => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": self.answer(method, message.get("params").unwrap_or(&Value::Null)),
            })),
            _ => None,
        }
    }

    fn record(&self, params: &Value) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return;
        };
        // The reader is the only writer of the sequence: the entry is in
        // place before waiters wake.
        let next = *self.seq.borrow() + 1;
        lock(&self.published).insert(
            uri.to_owned(),
            Published {
                version: params.get("version").and_then(Value::as_i64),
                seq: next,
                diagnostics: protocol::parse_list(params),
            },
        );
        self.seq.send_modify(|s| *s = next);
    }

    fn answer(&self, method: &str, params: &Value) -> Value {
        match method {
            "workspace/configuration" => Value::Array(
                params
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|item| self.section(item.get("section").and_then(Value::as_str)))
                    .collect(),
            ),
            "workspace/workspaceFolders" => self.folders.clone(),
            _ => Value::Null,
        }
    }

    fn section(&self, section: Option<&str>) -> Value {
        let Some(section) = section else {
            return self.settings.clone();
        };
        section
            .split('.')
            .try_fold(&self.settings, |value, key| value.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    }
}
