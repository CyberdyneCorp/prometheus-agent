//! The span model (spec: observability, Span model; Metrics catalog): a
//! session's trajectory events become OpenTelemetry spans nested by
//! causality (`session` > `turn` > `model.call` / `tool.call` >
//! `approval.wait`; `subagent` and `compression` under the turn), and the
//! tool, turn, budget and error metrics of the catalog are counted from the
//! same events.
//!
//! A [`SessionTrace`] exists for every session, export or not: it holds the
//! session's trace context, which its children (subagents, plugin and
//! bridge calls) and outbound calls read. Spans are built and queued only
//! when an [`Exporter`] is installed. Message content, tool arguments and
//! tool output become attributes only with `capture_content`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::metrics::{self, global};
use crate::trajectory::events::Event;
use crate::trajectory::payload::{Tokens, Usage};
use crate::trajectory::writer::Emit;

use super::context::{self, TraceContext, new_span_id, trace_id_for_root};
use super::proto::{self, KeyValue, kv, kv_double, kv_int};
use super::{Exporter, SPAN_NAMES};

/// The delegation tool, whose open span parents a child session.
const DELEGATE_TOOL: &str = "delegate_task";
/// The most text one content attribute carries.
const CONTENT_MAX_CHARS: usize = 8_192;

/// Who a session is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The profile.
    pub profile: String,
    /// The session.
    pub session_id: String,
    /// Its lineage root.
    pub root_session_id: String,
    /// Its surface (`cli`, `tui`, `gateway`, `cron`, `subagent`...).
    pub source: String,
}

#[derive(Debug, Clone)]
struct Open {
    name: &'static str,
    span_id: [u8; 8],
    parent: Option<[u8; 8]>,
    start: u64,
    attributes: Vec<KeyValue>,
    /// The tool of a `tool.call` span.
    tool: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    started: Option<u64>,
    ended: bool,
    turns: Vec<(String, Open)>,
    tools: Vec<(String, Open)>,
    approvals: HashMap<String, Open>,
    subagents: HashMap<String, Open>,
    aux: HashMap<String, Open>,
    compression: Option<Open>,
    jobs: HashMap<String, u64>,
}

/// A session's trace: its context and the spans of its open work.
#[derive(Debug)]
pub struct SessionTrace {
    identity: Identity,
    trace_id: [u8; 16],
    session_span: [u8; 8],
    parent: Option<[u8; 8]>,
    exporter: Option<Exporter>,
    state: Mutex<State>,
}

impl SessionTrace {
    /// The trace of a session starting now. Its parent span is `parent`
    /// when given (a cron run, an RPC request), else the open delegating
    /// tool call of `parent_session_id`'s live trace (a subagent), else
    /// none (a root span); the trace id is the parent's or the lineage's.
    #[must_use]
    pub fn open(
        identity: Identity,
        parent_session_id: Option<&str>,
        parent: Option<TraceContext>,
    ) -> Arc<Self> {
        let parent = parent.or_else(|| {
            parent_session_id
                .and_then(context::session)
                .map(|p| p.delegation_parent())
        });
        let trace_id = parent.map_or_else(
            || trace_id_for_root(&identity.root_session_id),
            |p| p.trace_id,
        );
        let trace = Arc::new(Self {
            trace_id,
            session_span: new_span_id(),
            parent: parent.map(|p| p.span_id),
            exporter: super::exporter(),
            state: Mutex::default(),
            identity,
        });
        context::register(&trace.identity.session_id, &trace);
        global().gauge_add(metrics::SESSIONS_ACTIVE, &[], 1.0);
        trace
    }

    /// The trace id as 32 hex digits.
    #[must_use]
    pub fn trace_hex(&self) -> String {
        context::hex(&self.trace_id)
    }

    /// The session span's context.
    #[must_use]
    pub fn context(&self) -> TraceContext {
        self.at(self.session_span)
    }

    /// The innermost open span: the latest tool call, else the latest
    /// turn, else the session.
    #[must_use]
    pub fn current(&self) -> TraceContext {
        let state = self.lock();
        let span = state
            .tools
            .last()
            .or(state.turns.last())
            .map_or(self.session_span, |(_, open)| open.span_id);
        self.at(span)
    }

    /// Where a child session hangs: the latest open `delegate_task` call,
    /// else [`current`](Self::current).
    #[must_use]
    pub fn delegation_parent(&self) -> TraceContext {
        let delegating = self
            .lock()
            .tools
            .iter()
            .rev()
            .find(|(_, open)| open.tool.as_deref() == Some(DELEGATE_TOOL))
            .map(|(_, open)| open.span_id);
        delegating.map_or_else(|| self.current(), |span| self.at(span))
    }

    /// Takes in one of the session's events.
    pub fn observe(&self, emit: &Emit) {
        let at = nanos(emit.ts);
        let mut state = self.lock();
        state.started.get_or_insert(at);
        match &emit.event {
            Event::SessionEnd(end) => self.end_session(&mut state, at, &end.reason),
            Event::TurnStart(_) | Event::TurnEnd(_) => self.turn(&mut state, emit, at),
            Event::UserMessage(m) => self.content(&mut state, emit, "prometheus.input", &m.text),
            Event::AssistantMessage(m) => {
                self.content(&mut state, emit, "prometheus.output", &m.text);
            }
            Event::ModelCall(call) => {
                let parent = self.turn_span(&state, emit);
                drop(state);
                self.model_call(call, parent, at);
            }
            Event::AuxCallStart(_) | Event::AuxCallEnd(_) => self.aux(&mut state, emit, at),
            Event::ToolCall(_) | Event::ToolResult(_) => self.tool(&mut state, emit, at),
            Event::ApprovalRequest(_) | Event::ApprovalDecision(_) => {
                self.approval(&mut state, emit, at);
            }
            Event::SubagentSpawned(_) | Event::SubagentFinished(_) => {
                self.subagent(&mut state, emit, at);
            }
            Event::CompressionStarted(_)
            | Event::CompressionCommitted(_)
            | Event::CompressionFailed(_) => self.compression(&mut state, emit, at),
            Event::BudgetExceeded(b) => {
                global().add(metrics::BUDGET_EXCEEDED, &[("scope", &b.scope)], 1.0);
            }
            Event::Error(e) => global().add(metrics::ERRORS, &[("kind", &e.kind)], 1.0),
            Event::EvolveJobStarted(_)
            | Event::EvolveStageFinished(_)
            | Event::EvolveRolloutFinished(_) => self.evolve(&mut state, emit, at),
            _ => {}
        }
    }

    fn at(&self, span_id: [u8; 8]) -> TraceContext {
        TraceContext {
            trace_id: self.trace_id,
            span_id,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn capture(&self) -> bool {
        self.exporter
            .as_ref()
            .is_some_and(Exporter::captures_content)
    }

    fn base_attributes(&self) -> Vec<KeyValue> {
        let id = &self.identity;
        vec![
            kv("prometheus.profile", &id.profile),
            kv("prometheus.session_id", &id.session_id),
            kv("prometheus.root_session_id", &id.root_session_id),
        ]
    }

    fn span(&self, name: &'static str, parent: [u8; 8], start: u64) -> Open {
        Open {
            name,
            span_id: new_span_id(),
            parent: Some(parent),
            start,
            attributes: self.base_attributes(),
            tool: None,
        }
    }

    /// Queues a finished span (when exporting).
    fn finish(&self, open: Open, end: u64, error: Option<String>) {
        let Some(exporter) = &self.exporter else {
            return;
        };
        debug_assert!(SPAN_NAMES.contains(&open.name));
        exporter.push_span(proto::Span {
            trace_id: self.trace_id.to_vec(),
            span_id: open.span_id.to_vec(),
            parent_span_id: open.parent.map(|p| p.to_vec()).unwrap_or_default(),
            name: open.name.to_owned(),
            kind: 1,
            start_time_unix_nano: open.start,
            end_time_unix_nano: end.max(open.start),
            attributes: open.attributes,
            status: Some(match error {
                Some(message) => proto::Status { message, code: 2 },
                None => proto::Status {
                    message: String::new(),
                    code: 0,
                },
            }),
        });
    }

    fn end_session(&self, state: &mut State, at: u64, reason: &str) {
        if std::mem::replace(&mut state.ended, true) {
            return;
        }
        global().gauge_add(metrics::SESSIONS_ACTIVE, &[], -1.0);
        let leftovers: Vec<Open> = state
            .tools
            .drain(..)
            .chain(state.turns.drain(..))
            .map(|(_, o)| o)
            .chain(state.approvals.drain().map(|(_, o)| o))
            .chain(state.subagents.drain().map(|(_, o)| o))
            .chain(state.aux.drain().map(|(_, o)| o))
            .chain(state.compression.take())
            .collect();
        for open in leftovers {
            self.finish(open, at, Some("unfinished at session end".to_owned()));
        }
        let mut session = Open {
            name: "session",
            span_id: self.session_span,
            parent: self.parent,
            start: state.started.unwrap_or(at),
            attributes: self.base_attributes(),
            tool: None,
        };
        session
            .attributes
            .push(kv("prometheus.source", &self.identity.source));
        session.attributes.push(kv("prometheus.end_reason", reason));
        self.finish(session, at, None);
    }

    /// The open span of `emit`'s turn, else the session span.
    fn turn_span(&self, state: &State, emit: &Emit) -> [u8; 8] {
        emit.turn_id
            .as_deref()
            .and_then(|id| state.turns.iter().find(|(t, _)| t == id))
            .map_or(self.session_span, |(_, open)| open.span_id)
    }

    fn turn(&self, state: &mut State, emit: &Emit, at: u64) {
        let Some(turn_id) = emit.turn_id.clone() else {
            return;
        };
        match &emit.event {
            Event::TurnStart(_) => {
                let mut open = self.span("turn", self.session_span, at);
                open.attributes.push(kv("prometheus.turn_id", &turn_id));
                state.turns.push((turn_id, open));
            }
            Event::TurnEnd(end) => {
                let Some(index) = state.turns.iter().position(|(t, _)| *t == turn_id) else {
                    return;
                };
                let (_, mut open) = state.turns.remove(index);
                global().add(metrics::TURNS, &[("outcome", &end.reason)], 1.0);
                global().observe(
                    metrics::TURN_DURATION_MS,
                    &[],
                    millis(at.saturating_sub(open.start)),
                );
                open.attributes
                    .push(kv("prometheus.exit_reason", &end.reason));
                open.attributes
                    .push(kv_int("prometheus.iterations", u64::from(end.iterations)));
                usage_attributes(&mut open.attributes, &end.usage);
                let error = (end.reason != "completed").then(|| end.reason.clone());
                self.finish(open, at, error);
            }
            _ => {}
        }
    }

    fn content(&self, state: &mut State, emit: &Emit, key: &str, text: &str) {
        if !self.capture() {
            return;
        }
        let span = self.turn_span(state, emit);
        if let Some((_, open)) = state.turns.iter_mut().find(|(_, o)| o.span_id == span) {
            open.attributes.push(kv(key, clip(text)));
        }
    }

    fn model_call(&self, call: &crate::trajectory::payload::ModelCall, parent: [u8; 8], at: u64) {
        global().observe(
            metrics::MODEL_LATENCY_MS,
            &[("provider", &call.provider), ("model", &call.model)],
            millis(call.latency_ms.saturating_mul(1_000_000)),
        );
        let mut open = self.span(
            "model.call",
            parent,
            at.saturating_sub(call.latency_ms * 1_000_000),
        );
        model_attributes(
            &mut open.attributes,
            &call.provider,
            &call.model,
            &call.tokens,
        );
        open.attributes
            .push(kv("prometheus.purpose", &call.purpose));
        open.attributes
            .push(kv("prometheus.call_id", &call.call_id));
        if let Some(cost) = call.cost_usd {
            open.attributes.push(kv_double("prometheus.cost_usd", cost));
        }
        self.finish(open, at, None);
    }

    fn aux(&self, state: &mut State, emit: &Emit, at: u64) {
        match &emit.event {
            Event::AuxCallStart(start) => {
                let mut open = self.span("model.call", self.turn_span(state, emit), at);
                open.attributes.push(kv("gen_ai.system", &start.provider));
                open.attributes
                    .push(kv("gen_ai.request.model", &start.model));
                open.attributes
                    .push(kv("prometheus.purpose", &start.purpose));
                state.aux.insert(start.call_id.clone(), open);
            }
            Event::AuxCallEnd(end) => {
                let Some(mut open) = state.aux.remove(&end.call_id) else {
                    return;
                };
                let tokens = &end.tokens;
                open.attributes
                    .push(kv_int("gen_ai.usage.input_tokens", tokens.input));
                open.attributes
                    .push(kv_int("gen_ai.usage.output_tokens", tokens.output));
                if let Some(cost) = end.cost_usd {
                    open.attributes.push(kv_double("prometheus.cost_usd", cost));
                }
                let error = end.error.as_ref().map(|f| f.kind.clone());
                self.finish(open, at, error);
            }
            _ => {}
        }
    }

    fn tool(&self, state: &mut State, emit: &Emit, at: u64) {
        match &emit.event {
            Event::ToolCall(call) => {
                let mut open = self.span("tool.call", self.turn_span(state, emit), at);
                open.attributes.push(kv("prometheus.tool", &call.tool));
                open.attributes
                    .push(kv("prometheus.call_id", &call.call_id));
                if self.capture() {
                    open.attributes.push(kv(
                        "prometheus.tool.arguments",
                        clip(&call.arguments.to_string()),
                    ));
                }
                open.tool = Some(call.tool.clone());
                state.tools.push((call.call_id.clone(), open));
            }
            Event::ToolResult(result) => {
                let outcome = if result.ok { "ok" } else { "error" };
                global().add(
                    metrics::TOOL_CALLS,
                    &[("tool", &result.tool), ("outcome", outcome)],
                    1.0,
                );
                if let Some(error) = &result.error {
                    global().add(metrics::ERRORS, &[("kind", &error.kind)], 1.0);
                }
                let Some(index) = state.tools.iter().position(|(id, _)| *id == result.call_id)
                else {
                    return;
                };
                let (_, mut open) = state.tools.remove(index);
                let duration = result.duration_ms.map_or_else(
                    || millis(at.saturating_sub(open.start)),
                    |ms| millis(ms * 1_000_000),
                );
                global().observe(
                    metrics::TOOL_DURATION_MS,
                    &[("tool", &result.tool)],
                    duration,
                );
                if let (true, Some(output)) = (self.capture(), &result.output) {
                    open.attributes
                        .push(kv("prometheus.tool.output", clip(output)));
                }
                let error = result.error.as_ref().map(|f| f.kind.clone());
                self.finish(
                    open,
                    at,
                    error.or_else(|| (!result.ok).then(|| "tool".to_owned())),
                );
            }
            _ => {}
        }
    }

    fn approval(&self, state: &mut State, emit: &Emit, at: u64) {
        match &emit.event {
            Event::ApprovalRequest(request) => {
                let parent = state
                    .tools
                    .iter()
                    .find(|(id, _)| *id == request.call_id)
                    .map_or_else(|| self.turn_span(state, emit), |(_, o)| o.span_id);
                let mut open = self.span("approval.wait", parent, at);
                open.attributes.push(kv("prometheus.tool", &request.tool));
                state.approvals.insert(request.request_id.clone(), open);
            }
            Event::ApprovalDecision(decision) => {
                if let Some(mut open) = state.approvals.remove(&decision.request_id) {
                    open.attributes
                        .push(kv("prometheus.decision", &decision.decision));
                    open.attributes
                        .push(kv("prometheus.decided_by", &decision.by));
                    self.finish(open, at, None);
                }
            }
            _ => {}
        }
    }

    fn subagent(&self, state: &mut State, emit: &Emit, at: u64) {
        match &emit.event {
            Event::SubagentSpawned(spawned) => {
                global().gauge_add(metrics::SUBAGENTS_ACTIVE, &[], 1.0);
                let parent = state
                    .tools
                    .iter()
                    .rev()
                    .find(|(_, o)| o.tool.as_deref() == Some(DELEGATE_TOOL))
                    .map_or_else(|| self.turn_span(state, emit), |(_, o)| o.span_id);
                let mut open = self.span("subagent", parent, at);
                open.attributes
                    .push(kv("prometheus.child_session_id", &spawned.child_session_id));
                open.attributes
                    .push(kv_int("prometheus.depth", u64::from(spawned.depth)));
                state.subagents.insert(spawned.subagent_id.clone(), open);
            }
            Event::SubagentFinished(finished) => {
                global().gauge_add(metrics::SUBAGENTS_ACTIVE, &[], -1.0);
                if let Some(mut open) = state.subagents.remove(&finished.subagent_id) {
                    open.attributes
                        .push(kv("prometheus.status", &finished.status));
                    usage_attributes(&mut open.attributes, &finished.usage);
                    let error = (finished.status != "completed").then(|| finished.status.clone());
                    self.finish(open, at, error);
                }
            }
            _ => {}
        }
    }

    fn compression(&self, state: &mut State, emit: &Emit, at: u64) {
        match &emit.event {
            Event::CompressionStarted(started) => {
                let mut open = self.span("compression", self.turn_span(state, emit), at);
                open.attributes
                    .push(kv("prometheus.trigger", &started.trigger));
                state.compression = Some(open);
            }
            Event::CompressionCommitted(_) | Event::CompressionFailed(_) => {
                if let Some(open) = state.compression.take() {
                    let failed = matches!(emit.event, Event::CompressionFailed(_));
                    self.finish(open, at, failed.then(|| "compression failed".to_owned()));
                }
            }
            _ => {}
        }
    }

    /// `evolve.stage` and `eval.task` spans, from the job's previous event
    /// to their `*.finished` event.
    fn evolve(&self, state: &mut State, emit: &Emit, at: u64) {
        let (job, name, detail) = match &emit.event {
            Event::EvolveJobStarted(job) => {
                state.jobs.insert(job.job_id.clone(), at);
                return;
            }
            Event::EvolveStageFinished(finished) => {
                (&finished.job_id, "evolve.stage", &finished.stage)
            }
            Event::EvolveRolloutFinished(rollout) => (&rollout.job_id, "eval.task", &rollout.task),
            _ => return,
        };
        let start = state.jobs.insert(job.clone(), at).unwrap_or(at);
        let mut open = self.span(name, self.session_span, start);
        open.attributes.push(kv("prometheus.evolve.job_id", job));
        open.attributes.push(kv("prometheus.name", detail));
        self.finish(open, at, None);
    }
}

impl Drop for SessionTrace {
    /// A session that never recorded `session.end` (its process stopped)
    /// still closes its spans.
    fn drop(&mut self) {
        let at = metrics::unix_nanos();
        let state = std::mem::take(self.state.get_mut().unwrap_or_else(PoisonError::into_inner));
        let mut state = state;
        self.end_session(&mut state, at, "closed");
    }
}

/// A span recorded outside a session's events: `cron.run` around a
/// scheduled run, `rpc.request` around a client request.
#[derive(Debug)]
pub struct Standalone {
    name: &'static str,
    context: TraceContext,
    parent: Option<[u8; 8]>,
    start: u64,
    attributes: Vec<KeyValue>,
}

impl Standalone {
    /// Starts span `name` with `context` (its trace and span id) under
    /// `parent` (whose trace id wins when given).
    #[must_use]
    pub fn start(name: &'static str, context: TraceContext, parent: Option<TraceContext>) -> Self {
        debug_assert!(SPAN_NAMES.contains(&name));
        Self {
            name,
            context: TraceContext {
                trace_id: parent.map_or(context.trace_id, |p| p.trace_id),
                span_id: context.span_id,
            },
            parent: parent.map(|p| p.span_id),
            start: metrics::unix_nanos(),
            attributes: Vec::new(),
        }
    }

    /// The span's context (what work under it hangs from).
    #[must_use]
    pub fn context(&self) -> TraceContext {
        self.context
    }

    /// Adds a string attribute.
    pub fn attribute(&mut self, key: &str, value: &str) {
        self.attributes.push(kv(key, value));
    }

    /// Ends the span now (queued when exporting).
    pub fn end(self, error: Option<String>) {
        let Some(exporter) = super::exporter() else {
            return;
        };
        exporter.push_span(proto::Span {
            trace_id: self.context.trace_id.to_vec(),
            span_id: self.context.span_id.to_vec(),
            parent_span_id: self.parent.map(|p| p.to_vec()).unwrap_or_default(),
            name: self.name.to_owned(),
            kind: 1,
            start_time_unix_nano: self.start,
            end_time_unix_nano: metrics::unix_nanos(),
            attributes: self.attributes,
            status: Some(proto::Status {
                code: if error.is_some() { 2 } else { 0 },
                message: error.unwrap_or_default(),
            }),
        });
    }
}

fn model_attributes(attributes: &mut Vec<KeyValue>, provider: &str, model: &str, tokens: &Tokens) {
    attributes.push(kv("gen_ai.system", provider));
    attributes.push(kv("gen_ai.request.model", model));
    attributes.push(kv_int(
        "gen_ai.usage.input_tokens",
        tokens.input + tokens.cache_read + tokens.cache_write,
    ));
    attributes.push(kv_int("gen_ai.usage.output_tokens", tokens.output));
}

fn usage_attributes(attributes: &mut Vec<KeyValue>, usage: &Usage) {
    attributes.push(kv_int("gen_ai.usage.input_tokens", usage.tokens.input));
    attributes.push(kv_int("gen_ai.usage.output_tokens", usage.tokens.output));
    if let Some(cost) = usage.cost_usd {
        attributes.push(kv_double("prometheus.cost_usd", cost));
    }
}

fn clip(text: &str) -> String {
    text.chars().take(CONTENT_MAX_CHARS).collect()
}

fn nanos(ts: jiff::Timestamp) -> u64 {
    u64::try_from(ts.as_nanosecond()).unwrap_or(0)
}

#[allow(clippy::cast_precision_loss)] // durations are far below 2^52 ns
fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}
