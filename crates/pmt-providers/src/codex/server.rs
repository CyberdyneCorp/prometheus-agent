//! One `codex app-server` process per session, speaking its JSON-RPC
//! protocol (one JSON object per line on stdio).
//!
//! The first call of a session starts the process, initialises it and
//! starts a Codex thread with the system prompt as developer instructions;
//! later calls reuse both. The thread keeps its own history, so a call
//! sends only the messages after the ones already delivered; when the
//! transcript got shorter than that (compression, a rewind) a new thread
//! starts with the whole transcript.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::classify::{ErrorClass, ProviderError};
use crate::request::{Part, Request, Role};

/// The running servers, by session (a call without a session gets a
/// process of its own).
static SERVERS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Server>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn failed(why: &str) -> ProviderError {
    ProviderError::new(ErrorClass::ServerError, format!("codex app-server: {why}"))
}

/// The server of `session`, started on first use.
pub(crate) fn of_session(program: &Path, session: Option<&str>) -> Arc<tokio::sync::Mutex<Server>> {
    let fresh = || Arc::new(tokio::sync::Mutex::new(Server::new(program)));
    let Some(session) = session else {
        return fresh();
    };
    let mut servers = SERVERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(servers.entry(session.to_owned()).or_insert_with(fresh))
}

/// Stops and forgets the server of `session` (the session ended).
pub fn close(session: &str) {
    SERVERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session);
}

/// A Codex plugin as `plugin/list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CodexPlugin {
    /// Its name.
    #[serde(alias = "id")]
    pub name: String,
    /// Installed on this machine.
    #[serde(default)]
    pub installed: bool,
    /// Still offered by Codex.
    #[serde(default)]
    pub available: bool,
}

/// The plugins the Codex at `program` reports (`plugin/list` on a
/// short-lived app-server).
///
/// # Errors
///
/// The server could not be started or answered with an error.
pub async fn plugins(program: &Path) -> Result<Vec<CodexPlugin>, String> {
    let mut server = Server::new(program);
    let listed = async {
        server.spawn().await?;
        server.call("plugin/list", json!({})).await
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), listed)
        .await
        .map_err(|_| "codex app-server did not answer plugin/list within 30 s".to_owned())?
        .map_err(|e| e.message)?;
    let list = result
        .get("plugins")
        .or_else(|| result.get("data"))
        .cloned();
    serde_json::from_value(list.unwrap_or_default())
        .map_err(|e| format!("plugin/list answered an unexpected shape: {e}"))
}

/// A server process and its thread.
#[derive(Debug)]
pub(crate) struct Server {
    program: PathBuf,
    io: Option<Io>,
    thread: Option<String>,
    /// Transcript messages the thread has seen.
    delivered: usize,
    next_id: u64,
}

#[derive(Debug)]
struct Io {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Server {
    fn new(program: &Path) -> Self {
        Self {
            program: program.to_path_buf(),
            io: None,
            thread: None,
            delivered: 0,
            next_id: 1,
        }
    }

    /// Starts Codex turn of `request` (process and thread first when
    /// needed) and answers its id; the turn's events follow on
    /// [`Server::next_frame`].
    pub(crate) async fn start_turn(&mut self, request: &Request) -> Result<(), ProviderError> {
        if self
            .io
            .as_mut()
            .is_none_or(|io| io.child.try_wait().ok().flatten().is_some())
        {
            self.spawn().await?;
        }
        if request.messages.len() < self.delivered {
            self.thread = None;
        }
        if self.thread.is_none() {
            self.start_thread(request).await?;
        }
        let text = new_input(request, self.delivered);
        let thread = self.thread.clone().unwrap_or_default();
        let input = json!([{"type": "text", "text": text}]);
        self.call("turn/start", json!({"threadId": thread, "input": input}))
            .await?;
        self.delivered = request.messages.len();
        Ok(())
    }

    async fn spawn(&mut self) -> Result<(), ProviderError> {
        let cwd = std::env::current_dir().map_err(|e| failed(&e.to_string()))?;
        let mut child = tokio::process::Command::new(&self.program)
            .arg("app-server")
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| failed(&format!("cannot start {}: {e}", self.program.display())))?;
        let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| failed("no stdout"))?;
        self.io = Some(Io {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        });
        self.thread = None;
        self.delivered = 0;
        let client = json!({"name": "prometheus", "title": "Prometheus", "version": env!("CARGO_PKG_VERSION")});
        self.call("initialize", json!({"clientInfo": client}))
            .await?;
        self.send(&json!({"jsonrpc": "2.0", "method": "initialized"}))
            .await
    }

    /// Starts a thread with the system prompt as its developer
    /// instructions (sent once per thread).
    async fn start_thread(&mut self, request: &Request) -> Result<(), ProviderError> {
        let instructions = request
            .system
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let cwd = std::env::current_dir().map_err(|e| failed(&e.to_string()))?;
        let mut params = json!({
            "cwd": cwd.to_string_lossy(),
            "developerInstructions": instructions,
            "approvalPolicy": "on-request",
        });
        if request.model != "default" {
            params["model"] = request.model.clone().into();
        }
        let result = self.call("thread/start", params).await?;
        let id = result["thread"]["id"]
            .as_str()
            .ok_or_else(|| failed("thread/start returned no thread id"))?;
        self.thread = Some(id.to_owned());
        self.delivered = 0;
        Ok(())
    }

    fn io(&mut self) -> Result<&mut Io, ProviderError> {
        self.io.as_mut().ok_or_else(|| failed("not running"))
    }

    pub(crate) async fn send(&mut self, frame: &Value) -> Result<(), ProviderError> {
        let mut line = frame.to_string();
        line.push('\n');
        let io = self.io()?;
        io.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| failed(&e.to_string()))
    }

    /// The next frame from the server.
    pub(crate) async fn next_frame(&mut self) -> Result<Value, ProviderError> {
        let io = self.io()?;
        loop {
            let Some(line) = io
                .lines
                .next_line()
                .await
                .map_err(|e| failed(&e.to_string()))?
            else {
                self.io = None;
                return Err(failed("the server exited"));
            };
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).map_err(|_| failed("a frame is not JSON"));
            }
        }
    }

    /// Sends request `method` and waits for its result. Notifications on
    /// the way are dropped; a server request on the way is declined (none
    /// is expected outside a turn).
    pub(crate) async fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, ProviderError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let frame = self.next_frame().await?;
            if frame.get("method").is_none() && frame["id"].as_u64() == Some(id) {
                return result_of(&frame, method);
            }
            if let (Some(_), Some(request_id)) = (frame["method"].as_str(), frame.get("id")) {
                let reply =
                    json!({"jsonrpc": "2.0", "id": request_id, "result": {"decision": "decline"}});
                self.send(&reply).await?;
            }
        }
    }
}

pub(crate) fn result_of(frame: &Value, method: &str) -> Result<Value, ProviderError> {
    if let Some(error) = frame.get("error") {
        return Err(failed(&format!(
            "{method} failed: {}",
            error["message"].as_str().unwrap_or("error")
        )));
    }
    Ok(frame["result"].clone())
}

/// The messages from `from` on as one turn's input: the user's text as
/// is, anything else labelled with its role. On a running thread the
/// assistant and tool rows after `from` are Codex's own answer and
/// actions, which the thread already holds.
fn new_input(request: &Request, from: usize) -> String {
    let mut text = String::new();
    let own = |role: &Role| from > 0 && *role != Role::User;
    for message in request.messages.iter().skip(from).filter(|m| !own(&m.role)) {
        let body = message
            .parts
            .iter()
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n");
        if body.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        match message.role {
            Role::User => text.push_str(&body),
            Role::Assistant => {
                let _ = write!(text, "Assistant: {body}");
            }
            Role::Tool => {
                let _ = write!(text, "Tool: {body}");
            }
        }
    }
    text
}

fn part_text(part: &Part) -> Option<String> {
    match part {
        Part::Text { text } => Some(text.clone()),
        Part::Image { .. } => None,
        Part::ToolCall {
            name, arguments, ..
        } => Some(format!("[called {name} {arguments}]")),
        Part::ToolResult { content, .. } => Some(format!("[result] {content}")),
    }
}
