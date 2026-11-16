//! The benchmark adapter protocol (spec: replay-and-evaluation, Benchmark
//! adapter protocol): an adapter answers `bench.describe`, `bench.tasks
//! {split}`, `bench.task {task_id}`, `bench.prepare {task_id, workspace}`,
//! `bench.deliverables {task_id, run_dir}`, `bench.preflight` and
//! `bench.score {task_id, run_dir}`.
//!
//! The protocol is the same over every [`Transport`]: a subprocess
//! speaking JSON-RPC 2.0 over stdio ([`stdio`], from
//! `<profile>/evals/bench/<name>/adapter.toml`, `command = ["python3",
//! "adapter.py"]` run in that directory), a WASM plugin of the `bench`
//! world or a Python-bridge plugin that registered the adapter (the host
//! builds those transports). A plugin has no filesystem of its own: it
//! reaches the call's directory through [`Files`] (the workspace during
//! `bench.prepare`, the run directory during `bench.deliverables` and
//! `bench.score`).
//!
//! The phases are types: an [`Adapter`] opened for execution has no way
//! to call `bench.score` (only [`Adapter::score`] in judge mode can), and
//! while executing, any request the adapter sends to push a score is
//! answered with error `-32002` ([`Host::answer`]), whatever the
//! transport. A null or failed score is an ungraded row.

pub mod files;
pub mod stdio;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use self::files::Files;
use crate::grader::Score;

/// The error code answering a score pushed during execution.
pub const SCORE_REFUSED: i64 = -32002;

/// The error code answering any other request to the host.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// How long one call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_mins(5);

/// A JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Its code.
    pub code: i64,
    /// Its message.
    pub message: String,
}

/// The host side of an adapter: what it answers to the requests the
/// adapter sends, and how many score pushes it refused.
#[derive(Debug, Clone)]
pub struct Host {
    name: String,
    phase: Phase,
    refused: Arc<AtomicU32>,
}

impl Host {
    /// The host of adapter `name` in `phase`.
    #[must_use]
    pub fn new(name: &str, phase: Phase) -> Self {
        Self {
            name: name.to_owned(),
            phase,
            refused: Arc::default(),
        }
    }

    /// The answer to request `method` the adapter sent: the host offers
    /// no method, and during execution a score push is refused with
    /// [`SCORE_REFUSED`].
    #[must_use]
    pub fn answer(&self, method: &str) -> RpcError {
        if method.contains("score") && self.phase == Phase::Execute {
            self.refused.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(adapter = %self.name, method, "score pushed during execution refused");
            return RpcError {
                code: SCORE_REFUSED,
                message: "scores are accepted only by prometheus eval judge".to_owned(),
            };
        }
        RpcError {
            code: METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
        }
    }

    /// Score pushes refused so far.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.refused.load(Ordering::SeqCst)
    }
}

/// One call's context: the host side and the call's directory.
#[derive(Debug, Clone)]
pub struct Call {
    /// The host side.
    pub host: Host,
    /// The directory the call is about.
    pub files: Files,
}

/// How the host reaches an adapter.
pub trait Transport: Send {
    /// One protocol call; requests the adapter sends meanwhile are
    /// answered by `call.host`.
    ///
    /// # Errors
    ///
    /// Why the call failed (the adapter's error included).
    fn call(&mut self, method: &str, params: &Value, call: &Call) -> Result<Value, String>;
}

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

/// An adapter, opened for one phase.
pub struct Adapter {
    name: String,
    dir: PathBuf,
    host: Host,
    transport: Box<dyn Transport>,
}

fn failed(name: &str, message: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::EVAL_BENCH_FAILED,
        format!("bench adapter {name}: {message}"),
    )
}

/// The directory of subprocess adapter `name`.
#[must_use]
pub fn dir(profile_dir: &Path, name: &str) -> PathBuf {
    profile_dir.join("evals").join("bench").join(name)
}

/// Whether the profile has subprocess adapter `name`.
#[must_use]
pub fn has_subprocess(profile_dir: &Path, name: &str) -> bool {
    dir(profile_dir, name).join("adapter.toml").is_file()
}

impl Adapter {
    /// Starts subprocess adapter `name` of the profile at `profile_dir`.
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
        let transport =
            stdio::Subprocess::spawn(&spec.command, &dir).map_err(|e| failed(name, e))?;
        Ok(Self::new(name, dir, phase, Box::new(transport)))
    }

    /// Adapter `name` (its files in `dir`) over `transport`.
    #[must_use]
    pub fn new(name: &str, dir: PathBuf, phase: Phase, transport: Box<dyn Transport>) -> Self {
        Self {
            name: name.to_owned(),
            dir,
            host: Host::new(name, phase),
            transport,
        }
    }

    /// The adapter directory (a plugin's: its plugin directory).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Scores the adapter pushed during execution (each refused).
    #[must_use]
    pub fn refused_pushes(&self) -> u32 {
        self.host.refused()
    }

    /// Calls `method`; the call's directory is `files`.
    fn call(&mut self, method: &str, params: &Value, files: Files) -> Result<Value, Error> {
        let call = Call {
            host: self.host.clone(),
            files,
        };
        self.transport
            .call(method, params, &call)
            .map_err(|e| failed(&self.name, e))
    }

    fn typed<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<T, Error> {
        let value = self.call(method, params, Files::none())?;
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
            Files::at(workspace.to_path_buf()),
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
            Files::at(run_dir.to_path_buf()),
        )
        .map(|_| ())
    }

    /// `bench.preflight`: `{ok, message}`.
    ///
    /// # Errors
    ///
    /// `eval.bench_failed`, also when it reports not ok.
    pub fn preflight(&mut self) -> Result<(), Error> {
        let reply = self.call("bench.preflight", &json!({}), Files::none())?;
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
        if self.host.phase != Phase::Judge {
            return Err("bench.score is called only by prometheus eval judge".to_owned());
        }
        let reply = self
            .call(
                "bench.score",
                &json!({"task_id": task_id, "run_dir": run_dir}),
                Files::at(run_dir.to_path_buf()),
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
