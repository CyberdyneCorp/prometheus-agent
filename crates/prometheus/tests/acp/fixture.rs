//! An ACP editor fixture: `prometheus acp` driven over its stdio the way
//! an editor does, against a daemon on a test home with the mock provider.
//! Every line the process writes on stdout is checked to be one JSON-RPC
//! 2.0 frame; the editor side of the protocol (permission cards, file
//! reads and writes, terminals) is answered by a replaceable responder.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::common;

/// How long a fixture waits for one frame.
const WAIT: Duration = Duration::from_mins(1);

/// Answers the agent's requests to the editor: the frame's `result`
/// (`Ok`) or `error` (`Err`).
pub(crate) type Responder = Box<dyn FnMut(&Value) -> Result<Value, Value> + Send>;

/// The default editor: dismisses every card, has no files or terminals.
fn dismissing() -> Responder {
    Box::new(|request: &Value| match request["method"].as_str() {
        Some("session/request_permission") => Ok(json!({"outcome": {"outcome": "cancelled"}})),
        Some(method) => Err(json!({"code": -32601, "message": format!("{method} not offered")})),
        None => Err(json!({"code": -32600, "message": "no method"})),
    })
}

/// A running `prometheus acp`.
pub(crate) struct Acp {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Value>,
    /// Notifications and agent requests seen while waiting.
    pub(crate) seen: Vec<Value>,
    next: i64,
    stderr: Arc<Mutex<String>>,
    /// Lines on stdout that were not JSON-RPC frames.
    strays: Arc<Mutex<Vec<String>>>,
    responder: Responder,
}

impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Acp {
    /// `prometheus [global] acp <args>` on `home` with `env` added.
    pub(crate) fn spawn(home: &Path, global: &[&str], args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = common::command(home);
        command
            .args(global)
            .arg("acp")
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, frames) = channel();
        let strays = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&strays);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str::<Value>(&line) {
                    Ok(frame) if frame["jsonrpc"] == "2.0" => {
                        let _ = tx.send(frame);
                    }
                    _ => kept.lock().unwrap().push(line),
                }
            }
        });
        let errors = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&errors);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut text = sink.lock().unwrap();
                text.push_str(&line);
                text.push('\n');
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            frames,
            seen: Vec::new(),
            next: 1,
            stderr: errors,
            strays,
            responder: dismissing(),
        }
    }

    /// Replaces the editor side.
    pub(crate) fn respond_with(&mut self, responder: Responder) {
        self.responder = responder;
    }

    /// Everything the process wrote on stderr so far.
    pub(crate) fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Asserts stdout carried only JSON-RPC frames.
    pub(crate) fn assert_only_frames(&self) {
        let strays = self.strays.lock().unwrap();
        assert!(strays.is_empty(), "non-frame lines on stdout: {strays:?}");
    }

    fn write(&mut self, frame: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{frame}").unwrap();
        stdin.flush().unwrap();
    }

    /// Sends a raw line.
    pub(crate) fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// Sends request `method`; returns its id.
    pub(crate) fn send(&mut self, method: &str, params: &Value) -> i64 {
        let id = self.next;
        self.next += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Sends notification `method`.
    pub(crate) fn notify(&mut self, method: &str, params: &Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Reads one frame, answering agent requests on the way; `None` on
    /// timeout or when the process closed stdout.
    fn next_frame(&mut self, until: Instant) -> Option<Value> {
        let left = until.saturating_duration_since(Instant::now());
        match self.frames.recv_timeout(left) {
            Ok(frame) => {
                if frame.get("method").is_some() && frame.get("id").is_some() {
                    let answer = (self.responder)(&frame);
                    let reply = match answer {
                        Ok(result) => {
                            json!({"jsonrpc": "2.0", "id": frame["id"], "result": result})
                        }
                        Err(error) => json!({"jsonrpc": "2.0", "id": frame["id"], "error": error}),
                    };
                    self.write(&reply);
                }
                Some(frame)
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Waits for the response to request `id`.
    pub(crate) fn response(&mut self, id: i64) -> Value {
        let until = Instant::now() + WAIT;
        while let Some(frame) = self.next_frame(until) {
            if frame["id"] == id && frame.get("method").is_none() {
                return frame;
            }
            self.seen.push(frame);
        }
        panic!(
            "no response to request {id}; seen {:?}\nstderr: {}",
            self.seen,
            self.stderr()
        );
    }

    /// Sends request `method` and waits for its response.
    pub(crate) fn call(&mut self, method: &str, params: &Value) -> Value {
        let id = self.send(method, params);
        self.response(id)
    }

    /// The `result` of a call, panicking on an error.
    pub(crate) fn ok(&mut self, method: &str, params: &Value) -> Value {
        let response = self.call(method, params);
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}\nstderr: {}",
            self.stderr()
        );
        response["result"].clone()
    }

    /// Waits until a frame matching `wanted` was seen (already or later).
    pub(crate) fn wait_for(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.seen.iter().find(|f| wanted(f)) {
            return found.clone();
        }
        let until = Instant::now() + WAIT;
        while let Some(frame) = self.next_frame(until) {
            let hit = wanted(&frame);
            self.seen.push(frame.clone());
            if hit {
                return frame;
            }
        }
        panic!(
            "never saw {what}; seen {:?}\nstderr: {}",
            self.seen,
            self.stderr()
        );
    }

    /// Reads frames for `time`, answering agent requests.
    pub(crate) fn pump(&mut self, time: Duration) {
        let until = Instant::now() + time;
        while let Some(frame) = self.next_frame(until) {
            self.seen.push(frame);
        }
    }

    /// `initialize` with `capabilities`.
    pub(crate) fn initialize(&mut self, capabilities: &Value) -> Value {
        self.ok(
            "initialize",
            &json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
        )
    }

    /// A new session working in `cwd`; its id.
    pub(crate) fn new_session(&mut self, cwd: &Path) -> String {
        let result = self.ok("session/new", &json!({"cwd": cwd, "mcpServers": []}));
        result["sessionId"].as_str().unwrap().to_owned()
    }

    /// Sends `text` as a prompt of `session`; the request id.
    pub(crate) fn send_prompt(&mut self, session: &str, text: &str) -> i64 {
        self.send(
            "session/prompt",
            &json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// The updates of `session` seen so far.
    pub(crate) fn updates(&self, session: &str) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|f| f["method"] == "session/update" && f["params"]["sessionId"] == session)
            .map(|f| f["params"]["update"].clone())
            .collect()
    }

    /// The agent text `session` was shown, chunks joined.
    pub(crate) fn agent_text(&self, session: &str) -> String {
        self.updates(session)
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str())
            .collect()
    }

    /// The agent's requests to the editor seen so far, by method.
    pub(crate) fn requests(&self, method: &str) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|f| f["method"] == method && f.get("id").is_some())
            .cloned()
            .collect()
    }

    /// Closes stdin (the editor went away) and waits for the exit.
    pub(crate) fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "acp did not exit after stdin closed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills the process (an editor crashing).
    pub(crate) fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A test home on the mock provider with a working directory `work`.
pub(crate) struct Home {
    pub(crate) dir: common::TestHome,
    pub(crate) work: PathBuf,
}

impl Home {
    pub(crate) fn new() -> Self {
        let dir = common::mock_home();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        Self { dir, work }
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Appends `text` to the default profile's `config.toml`.
    pub(crate) fn config(&self, text: &str) {
        common::append_config(self.path(), text);
    }

    /// Writes a mock script and makes it the profile's.
    pub(crate) fn script(&self, script: &str) {
        let path = self.path().join("mock.toml");
        let fresh = !path.exists();
        std::fs::write(&path, script).unwrap();
        if !fresh {
            return;
        }
        self.config(&format!(
            "\n[providers.mock]\nscript = \"{}\"\n",
            path.to_string_lossy().replace('\\', "/")
        ));
    }

    /// `prometheus rpc call <method> <params>`'s JSON answer.
    pub(crate) fn rpc(&self, method: &str, params: &Value) -> Value {
        let out = common::run(self.path(), &["rpc", "call", method, &params.to_string()]);
        serde_json::from_str(&common::ok(&out)).unwrap()
    }
}
