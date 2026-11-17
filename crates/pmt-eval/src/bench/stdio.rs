//! The subprocess transport: JSON-RPC 2.0 over stdio, one message per
//! line. Requests the adapter sends while a call is open are answered by
//! the host side ([`Host::answer`]).

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};

use serde_json::{Value, json};

use super::{CALL_TIMEOUT, Call, Transport};

/// A running adapter process.
pub struct Subprocess {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Subprocess {
    /// Starts `command` in `dir`.
    ///
    /// # Errors
    ///
    /// Why it did not start.
    pub fn spawn(command: &[String], dir: &Path) -> Result<Self, String> {
        let (program, args) = command
            .split_first()
            .ok_or("adapter.toml has an empty command")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 0,
        })
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("write: {e}"))
    }

    /// Answers a request the adapter sent (a notification gets none).
    fn answer(&mut self, message: &Value, call: &Call) -> Result<(), String> {
        let method = message["method"].as_str().unwrap_or_default();
        let error = call.host.answer(method);
        match message.get("id") {
            Some(id) => self.send(&json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": error.code, "message": error.message},
            })),
            None => Ok(()),
        }
    }
}

impl Transport for Subprocess {
    fn call(&mut self, method: &str, params: &Value, call: &Call) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let line = match self.lines.recv_timeout(CALL_TIMEOUT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err(format!("{method} timed out")),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("exited during {method}"));
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                return Err(format!("{method}: not JSON-RPC: {line}"));
            };
            if message.get("method").is_some() {
                self.answer(&message, call)?;
                continue;
            }
            if message["id"] != json!(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("{method}: {error}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for Subprocess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
