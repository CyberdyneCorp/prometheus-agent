//! What the engine asks of its host: rollouts (headless sessions that try
//! a harness on the task) and improver sessions (sessions confined to an
//! evidence directory). The engine owns everything else — evidence,
//! acceptance, the ledger, caching and selection — so it runs the same
//! against the real session factory and against test doubles.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use pmt_core::cancel::CancelToken;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A boxed `Send` future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A call to an `effect = external` tool a rollout recorded instead of
/// running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedCall {
    /// The tool.
    pub tool: String,
    /// Its arguments.
    pub arguments: Value,
}

/// What one entry of a rollout's trajectory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// The task as the user message.
    User,
    /// Assistant text.
    Assistant,
    /// A tool call (`name` and arguments as text).
    ToolCall,
    /// A tool observation.
    ToolResult,
    /// Something the harness or the host noted (a denied approval).
    Note,
}

/// One entry of a rollout's trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// What it is.
    pub kind: EntryKind,
    /// The tool, for calls and results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Its text.
    pub text: String,
}

impl Entry {
    /// An entry of `kind` with `text`.
    #[must_use]
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            tool: None,
            text: text.into(),
        }
    }

    /// The same entry naming `tool`.
    #[must_use]
    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }
}

/// One rollout to run.
#[derive(Debug, Clone, Copy)]
pub struct RolloutRequest<'a> {
    /// The job.
    pub job_id: &'a str,
    /// `base1`, `inv2`, `agg1`...
    pub label: &'a str,
    /// The frozen task (`<job>/task`).
    pub task: &'a Path,
    /// The candidate harness tree to run with.
    pub harness: &'a Path,
    /// The rollout seed (part of its cache key).
    pub seed: u64,
    /// Where the rollout may put image deliverables (at most 8 are kept).
    pub images: &'a Path,
    /// Stops the rollout at its next model-call boundary.
    pub cancel: &'a CancelToken,
}

/// What a rollout produced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RolloutOutcome {
    /// It ended with a final assistant message.
    pub ok: bool,
    /// Why it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The final assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// The trajectory, in order.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// The unified diff of the rollout workspace.
    #[serde(default)]
    pub diff: String,
    /// External calls recorded instead of run.
    #[serde(default)]
    pub external_calls: Vec<RecordedCall>,
    /// Spend in US dollars.
    #[serde(default)]
    pub usd: f64,
}

impl RolloutOutcome {
    /// A failed rollout.
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// Whether it produced a deliverable (a final message, a workspace
    /// change or a recorded external call).
    #[must_use]
    pub fn delivered(&self) -> bool {
        self.final_message
            .as_deref()
            .is_some_and(|m| !m.trim().is_empty())
            || !self.diff.trim().is_empty()
            || !self.external_calls.is_empty()
    }
}

/// Runs rollouts.
pub trait Rollouts: Send + Sync {
    /// Runs one rollout. Failures are outcomes (`ok = false`), never
    /// errors: a rollout never aborts the job.
    fn run<'a>(&'a self, request: RolloutRequest<'a>) -> BoxFuture<'a, RolloutOutcome>;

    /// The model rollouts run on (part of the rollout cache key).
    fn model_id(&self) -> String;
}

/// What an improver session does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// A proposal stage (`inv<i>`, `dom<i>`).
    Propose,
    /// A composition or checkpoint (`agg<i>`, `c<t>`).
    Compose,
    /// The blind selector.
    Select,
    /// The reflection that distils a lesson.
    Reflect,
    /// The domain classification of a job that names none.
    Classify,
}

/// One improver session to run.
#[derive(Debug, Clone, Copy)]
pub struct ImproverRequest<'a> {
    /// The job.
    pub job_id: &'a str,
    /// The stage label (`inv1`, `select-parallel`...).
    pub label: &'a str,
    /// What it does.
    pub kind: SessionKind,
    /// Its system prompt.
    pub system: &'a str,
    /// Its mission (the user message).
    pub mission: &'a str,
    /// Its private working directory (holding `context/`); its file tools
    /// are confined to it.
    pub workspace: &'a Path,
    /// Model calls it may make.
    pub max_iterations: u32,
    /// Its wall-clock limit.
    pub timeout: Duration,
    /// Stops it at its next model-call boundary.
    pub cancel: &'a CancelToken,
}

/// A session's reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImproverReply {
    /// The final assistant text (empty: no reply).
    pub text: String,
    /// Spend in US dollars.
    pub usd: f64,
}

/// A session that failed.
#[derive(Debug, Clone, PartialEq)]
pub struct ImproverFailure {
    /// A transport failure (retried once, never a stage retry).
    pub transport: bool,
    /// What happened.
    pub message: String,
    /// What it cost before failing.
    pub usd: f64,
}

/// Runs improver sessions.
pub trait Improver: Send + Sync {
    /// Runs one session.
    fn run<'a>(
        &'a self,
        request: ImproverRequest<'a>,
    ) -> BoxFuture<'a, Result<ImproverReply, ImproverFailure>>;

    /// The improver model (part of the session cache key).
    fn model_id(&self) -> String;
}
