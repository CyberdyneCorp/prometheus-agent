//! What a replayed session reads instead of the world (spec:
//! replay-and-evaluation, Replay modes; Divergence detection; Replay has
//! no side effects).
//!
//! The [`Replayer`] holds the recording's responses (by request hash),
//! tool results (by call id) and prompt builds. Every model call and tool
//! call of the replay is one step; a model request whose hash the
//! recording lacks, or a tool call whose name or canonical arguments
//! differ from the recorded call of that id, is the divergence: it is
//! reported once with a unified diff of the canonical forms, and then the
//! replay stops (the turn is cancelled and the call refused) or, with
//! `--on-divergence continue`, everything after it runs live.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use pmt_agent::prompt::SystemPrompt;
use pmt_agent::prompt::pins::{self, PromptPins};
use pmt_agent::turn::model::RequestedCall;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::compare;
use pmt_eval::recording::{Recording, member};
use pmt_obs::recording::{Prompt, Tool};
use pmt_providers::calls::{Answer, CallKey, Recorded, ReplaySource};
use pmt_providers::classify::{ErrorClass, ProviderError};
use pmt_providers::request;
use pmt_providers::resolve::BoxFuture;
use pmt_tools::ToolResult;
use serde_json::Value;

use super::{Mode, OnDivergence, ToolAnswer, ToolReplay};

/// The first difference between a replay and its recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The step (model and tool calls counted from 1).
    pub step: usize,
    /// `model.call` or `tool.call`.
    pub kind: &'static str,
    /// The unified diff of the recorded and replayed canonical forms.
    pub diff: String,
}

impl Divergence {
    /// `divergence at step <n> (<kind>)`.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("divergence at step {} ({})", self.step, self.kind)
    }
}

#[derive(Debug, Default)]
struct Progress {
    steps: usize,
    served_calls: usize,
    divergence: Option<Divergence>,
    /// A divergence fell through to the live provider and tools.
    live: bool,
}

/// One recorded request and its responses in call order (a retried
/// request failed before it answered); the last one answers every
/// further call.
struct RecordedCall {
    request: Value,
    responses: VecDeque<Recorded>,
}

impl RecordedCall {
    fn next(&mut self) -> Recorded {
        if self.responses.len() > 1 {
            return self.responses.pop_front().unwrap_or_else(empty);
        }
        self.responses.front().cloned().unwrap_or_else(empty)
    }
}

fn empty() -> Recorded {
    Recorded {
        chunks: Vec::new(),
        usage: pmt_providers::Usage::default(),
        failure: None,
    }
}

/// Answers a replay's model calls, tool calls and prompt builds.
pub struct Replayer {
    mode: Mode,
    on_divergence: OnDivergence,
    calls: Mutex<HashMap<String, RecordedCall>>,
    /// Recorded request hashes in call order (the diff's candidates).
    order: Vec<String>,
    tools: HashMap<String, Tool>,
    /// Each recorded session's prompt builds, in order (a replay
    /// recreates the session under its recorded id).
    prompts: Mutex<HashMap<String, VecDeque<Prompt>>>,
    /// The tool schemas the recording's first request carried.
    schemas: Vec<request::Tool>,
    progress: Mutex<Progress>,
    /// Stops the running turn at a divergence that stops the replay.
    stop: CancelToken,
}

impl std::fmt::Debug for Replayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replayer")
            .field("mode", &self.mode)
            .field("calls", &self.order.len())
            .finish_non_exhaustive()
    }
}

fn malformed(why: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::REPLAY_MALFORMED,
        format!("not a replayable recording: {why}"),
    )
}

impl Replayer {
    /// The replayer of `recording`; `stop` is cancelled at a stopping
    /// divergence.
    ///
    /// # Errors
    ///
    /// `replay.malformed` when a member does not parse.
    pub fn new(
        recording: &Recording,
        (mode, on_divergence): (Mode, OnDivergence),
        stop: CancelToken,
    ) -> Result<Self, Error> {
        let mut calls = HashMap::new();
        let mut order = Vec::new();
        let mut tools = HashMap::new();
        let mut prompts = HashMap::new();
        for session in &recording.manifest.session_ids {
            let listed: Vec<Value> = recording.json(&member::calls(session))?.unwrap_or_default();
            for entry in listed {
                let hash = entry["request_hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if calls.contains_key(&hash) {
                    continue;
                }
                let responses: VecDeque<Recorded> = recording
                    .json(&member::response(&hash))?
                    .ok_or_else(|| malformed(format!("no response for {hash}")))?;
                let request = recording.json(&member::request(&hash))?.unwrap_or_default();
                order.push(hash.clone());
                calls.insert(hash, RecordedCall { request, responses });
            }
            for line in recording
                .get(&member::tools(session))
                .map(String::from_utf8_lossy)
                .unwrap_or_default()
                .lines()
            {
                let tool: Tool = serde_json::from_str(line).map_err(malformed)?;
                tools.insert(tool.call_id.clone(), tool);
            }
            let built: Vec<Prompt> = recording
                .json(&member::prompts(session))?
                .unwrap_or_default();
            prompts.insert(session.clone(), built.into());
        }
        let schemas = order
            .first()
            .and_then(|hash| calls.get(hash))
            .and_then(|c| serde_json::from_value(c.request["tools"].clone()).ok())
            .unwrap_or_default();
        let calls = Mutex::new(calls);
        Ok(Self {
            mode,
            on_divergence,
            calls,
            order,
            tools,
            prompts: Mutex::new(prompts),
            schemas,
            progress: Mutex::default(),
            stop,
        })
    }

    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The divergence, once one happened.
    #[must_use]
    pub fn divergence(&self) -> Option<Divergence> {
        self.progress().divergence.clone()
    }

    /// Model calls served from the recording.
    #[must_use]
    pub fn served_calls(&self) -> usize {
        self.progress().served_calls
    }

    /// Records the divergence at `step`; whether the replay goes on live.
    fn diverged(&self, progress: &mut Progress, divergence: Divergence) -> bool {
        tracing::warn!(
            step = divergence.step,
            kind = divergence.kind,
            "replay diverged"
        );
        progress.divergence.get_or_insert(divergence);
        match self.on_divergence {
            OnDivergence::Continue => {
                progress.live = true;
                true
            }
            OnDivergence::Stop => {
                self.stop.cancel();
                false
            }
        }
    }

    /// The recorded request closest to `canonical`: the longest common
    /// message prefix, the earliest on a tie.
    fn closest(&self, calls: &HashMap<String, RecordedCall>, canonical: &Value) -> Value {
        let empty = Vec::new();
        let messages = canonical["messages"].as_array().unwrap_or(&empty);
        let shared = |request: &Value| {
            request["messages"].as_array().map_or(0, |recorded| {
                recorded
                    .iter()
                    .zip(messages)
                    .take_while(|(a, b)| a == b)
                    .count()
            })
        };
        self.order
            .iter()
            .filter_map(|hash| calls.get(hash))
            .map(|call| &call.request)
            .rev()
            .max_by_key(|request| shared(request))
            .cloned()
            .unwrap_or_default()
    }
}

impl ReplaySource for Replayer {
    fn answer<'a>(&'a self, key: &'a CallKey) -> BoxFuture<'a, Answer> {
        Box::pin(async move {
            let mut progress = self.progress();
            progress.steps += 1;
            if progress.live || self.mode == Mode::ModelLive {
                return Answer::Live;
            }
            let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(call) = calls.get_mut(&key.hash) {
                progress.served_calls += 1;
                return Answer::Serve(call.next());
            }
            let recorded = self.closest(&calls, &key.canonical);
            let divergence = Divergence {
                step: progress.steps,
                kind: "model.call",
                diff: compare::unified(
                    &compare::pretty(&recorded),
                    &compare::pretty(&key.canonical),
                ),
            };
            let headline = divergence.headline();
            if self.diverged(&mut progress, divergence) {
                return Answer::Live;
            }
            Answer::Refuse(ProviderError::refused(
                ErrorClass::InvalidRequest,
                &Error::new(&codes::REPLAY_DIVERGED, headline),
            ))
        })
    }
}

/// Arguments as the recording keeps them: parsed JSON, else the text.
fn arguments(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

impl ToolReplay for Replayer {
    fn answer(&self, call: &RequestedCall) -> ToolAnswer {
        let mut progress = self.progress();
        progress.steps += 1;
        if progress.live || self.mode == Mode::ToolsLive {
            return ToolAnswer::Live;
        }
        let args = arguments(&call.arguments);
        let recorded = self.tools.get(&call.id);
        if let Some(tool) = recorded.filter(|t| t.name == call.name && t.arguments == args) {
            let result = serde_json::from_value(tool.result.clone())
                .unwrap_or_else(|_| ToolResult::error("the recorded result does not parse"));
            return ToolAnswer::Serve(result);
        }
        let form = |name: &str, arguments: &Value| {
            compare::pretty(&serde_json::json!({"name": name, "arguments": arguments}))
        };
        let divergence = Divergence {
            step: progress.steps,
            kind: "tool.call",
            diff: compare::unified(
                &recorded
                    .map(|t| form(&t.name, &t.arguments))
                    .unwrap_or_default(),
                &form(&call.name, &args),
            ),
        };
        let headline = divergence.headline();
        if self.diverged(&mut progress, divergence) {
            return ToolAnswer::Live;
        }
        // A strict replay never runs an unrecorded call.
        ToolAnswer::Serve(ToolResult::error(format!(
            "{headline}: the recording has no result for this call"
        )))
    }

    fn recorded_tools(&self) -> Vec<request::Tool> {
        self.schemas.clone()
    }
}

impl PromptPins for Replayer {
    fn pin(&self, session: SessionId, built: SystemPrompt) -> SystemPrompt {
        let next = self
            .prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&session.to_string())
            .and_then(VecDeque::pop_front);
        match next {
            Some(recorded) => pins::pin(&built, &recorded.sections),
            None => built,
        }
    }
}
