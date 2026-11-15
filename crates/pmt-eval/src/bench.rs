//! The benchmark adapter protocol (spec: replay-and-evaluation, Benchmark
//! adapter protocol): an adapter is a subprocess speaking JSON-RPC 2.0
//! over stdio, one message per line, and answers `bench.describe`,
//! `bench.tasks {split}`, `bench.task {task_id}`, `bench.prepare
//! {task_id, workspace}`, `bench.deliverables {task_id, run_dir}`,
//! `bench.preflight` and `bench.score {task_id, run_dir}`.
//!
//! The phases are types: an [`Adapter`] opened for execution has no way
//! to call `bench.score` (only [`Adapter::score`] in judge mode can), and
//! while executing, any request the adapter sends to push a score is
//! answered with error `-32002`. A null or failed score is an ungraded
//! row.
//!
//! Adapters live in `<profile>/evals/bench/<name>/adapter.toml`
//! (`command = ["python3", "adapter.py"]`, run in that directory).

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::grader::Score;

/// The error code answering a score pushed during execution.
pub const SCORE_REFUSED: i64 = -32002;

/// How long one call may take.
const CALL_TIMEOUT: Duration = Duration::from_mins(5);

/// `adapter.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterFile {
    /// The command and its arguments, run in the adapter directory.
    pub command: Vec<String>,
}

/// `bench.describe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Describe {
    /// The benchmark's name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// Its domains.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Globs of answer material.
    #[serde(default)]
    pub leak_globs: Vec<String>,
}

/// A file `bench.task` lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFile {
    /// Its path in the workspace.
    pub path: String,
    /// Its text.
    pub content: String,
}

/// `bench.task`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTask {
    /// The prompt, grading criteria removed.
    pub prompt: String,
    /// Its domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Files for the workspace.
    #[serde(default)]
    pub files: Vec<TaskFile>,
    /// Its time limit.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// What the adapter is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// `eval run`: `bench.score` is never called and pushed scores are
    /// refused.
    Execute,
    /// `eval judge`.
    Judge,
}

/// A running adapter.
pub struct Adapter {
    name: String,
    dir: PathBuf,
    phase: Phase,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    refused: u32,
}

fn failed(name: &str, message: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::EVAL_BENCH_FAILED,
        format!("bench adapter {name}: {message}"),
    )
}

/// The directory of adapter `name`.
#[must_use]
pub fn dir(profile_dir: &Path, name: &str) -> PathBuf {
    profile_dir.join("evals").join("bench").join(name)
}

impl Adapter {
    /// Starts adapter `name` of the profile at `profile_dir`.
    ///
    /// # Errors
    ///
    /// `eval.suite_not_found` (66) without an `adapter.toml`;
    /// `eval.bench_failed` (69) when it does not start.
    pub fn open(profile_dir: &Path, name: &str, phase: Phase) -> Result<Self, Error> {
        let dir = dir(profile_dir, name);
        let file = dir.join("adapter.toml");
        let text = std::fs::read_to_string(&file).map_err(|_| {
            Error::new(
                &codes::EVAL_SUITE_NOT_FOUND,
                format!(
                    "no benchmark adapter '{name}' ({} is missing)",
                    file.display()
                ),
            )
        })?;
        let spec: AdapterFile = toml::from_str(&text).map_err(|e| failed(name, e))?;
        let (program, args) = spec
            .command
            .split_first()
            .ok_or_else(|| failed(name, "adapter.toml has an empty command"))?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| failed(name, format!("cannot start {program}: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| failed(name, "no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| failed(name, "no stdout"))?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            name: name.to_owned(),
            dir,
            phase,
            child,
            stdin,
            lines,
            next_id: 0,
            refused: 0,
        })
    }

    /// The adapter directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Scores the adapter pushed during execution (each refused).
    #[must_use]
    pub const fn refused_pushes(&self) -> u32 {
        self.refused
    }

    fn send(&mut self, message: &Value) -> Result<(), Error> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| failed(&self.name, format!("write: {e}")))
    }

    /// Calls `method`; adapter requests arriving meanwhile are answered
    /// (a score push is refused with [`SCORE_REFUSED`]).
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, Error> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let line = match self.lines.recv_timeout(CALL_TIMEOUT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(failed(&self.name, format!("{method} timed out")));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(failed(&self.name, format!("exited during {method}")));
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                return Err(failed(
                    &self.name,
                    format!("{method}: not JSON-RPC: {line}"),
                ));
            };
            if message.get("method").is_some() {
                self.answer_request(&message)?;
                continue;
            }
            if message["id"] != json!(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(failed(&self.name, format!("{method}: {error}")));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Answers a request the adapter sent: no host method is offered, and
    /// during execution a score push is refused with `-32002`.
    fn answer_request(&mut self, message: &Value) -> Result<(), Error> {
        let method = message["method"].as_str().unwrap_or_default();
        let pushes_score = method.contains("score");
        let (code, text) = if pushes_score && self.phase == Phase::Execute {
            self.refused += 1;
            tracing::warn!(adapter = %self.name, method, "score pushed during execution refused");
            (
                SCORE_REFUSED,
                "scores are accepted only by prometheus eval judge",
            )
        } else {
            (-32601, "method not found")
        };
        match message.get("id") {
            Some(id) => self.send(
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}}),
            ),
            None => Ok(()),
        }
    }

    fn typed<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<T, Error> {
        let value = self.call(method, params)?;
        serde_json::from_value(value).map_err(|e| failed(&self.name, format!("{method}: {e}")))
    }

    /// `bench.describe`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`.
    pub fn describe(&mut self) -> Result<Describe, Error> {
        self.typed("bench.describe", &json!({}))
    }

    /// `bench.tasks`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`.
    pub fn tasks(&mut self, split: Option<&str>) -> Result<Vec<String>, Error> {
        self.typed("bench.tasks", &json!({"split": split}))
    }

    /// `bench.task`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`.
    pub fn task(&mut self, task_id: &str) -> Result<BenchTask, Error> {
        self.typed("bench.task", &json!({"task_id": task_id}))
    }

    /// `bench.prepare`: the adapter fills `workspace`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`.
    pub fn prepare(&mut self, task_id: &str, workspace: &Path) -> Result<(), Error> {
        self.call(
            "bench.prepare",
            &json!({"task_id": task_id, "workspace": workspace}),
        )
        .map(|_| ())
    }

    /// `bench.deliverables`: the adapter collects what it grades into
    /// `run_dir`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`.
    pub fn deliverables(&mut self, task_id: &str, run_dir: &Path) -> Result<(), Error> {
        self.call(
            "bench.deliverables",
            &json!({"task_id": task_id, "run_dir": run_dir}),
        )
        .map(|_| ())
    }

    /// `bench.preflight`: `{ok, message}`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`, also when it reports not ok.
    pub fn preflight(&mut self) -> Result<(), Error> {
        let reply = self.call("bench.preflight", &json!({}))?;
        if reply.get("ok").and_then(Value::as_bool) == Some(false) {
            let why = reply["message"].as_str().unwrap_or("not ready");
            return Err(failed(&self.name, format!("preflight: {why}")));
        }
        Ok(())
    }

    /// `bench.score` (judge phase only): `None` for a null score.
    ///
    /// # Errors
    ///
    /// Why the row is ungraded.
    pub fn score(&mut self, task_id: &str, run_dir: &Path) -> Result<Option<Score>, String> {
        if self.phase != Phase::Judge {
            return Err("bench.score is called only by prometheus eval judge".to_owned());
        }
        let reply = self
            .call(
                "bench.score",
                &json!({"task_id": task_id, "run_dir": run_dir}),
            )
            .map_err(|e| e.message().to_owned())?;
        if reply.is_null() {
            return Ok(None);
        }
        serde_json::from_value(reply)
            .map(Some)
            .map_err(|e| format!("bench.score: {e}"))
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `bench:<name>[:<split>]` → `(name, split)`.
#[must_use]
pub fn parse_ref(spec: &str) -> Option<(&str, Option<&str>)> {
    let rest = spec.strip_prefix("bench:")?;
    let mut parts = rest.splitn(2, ':');
    let name = parts.next().filter(|n| !n.is_empty())?;
    Some((name, parts.next().filter(|s| !s.is_empty())))
}

#[cfg(test)]
mod tests {
    use super::parse_ref;

    #[test]
    fn bench_refs_name_an_adapter_and_a_split() {
        assert_eq!(parse_ref("bench:example"), Some(("example", None)));
        assert_eq!(
            parse_ref("bench:example:dev"),
            Some(("example", Some("dev")))
        );
        assert_eq!(parse_ref("smoke"), None);
        assert_eq!(parse_ref("bench:"), None);
    }
}
