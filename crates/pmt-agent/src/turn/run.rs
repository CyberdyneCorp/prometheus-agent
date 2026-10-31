//! One turn, phase by phase (spec: agent-turn-loop, Iteration phases).
//!
//! Each iteration runs, in order: the interrupt and budget check, steer
//! delivery and transcript repair (seams of later tasks), request assembly
//! from the frozen prompt and the stored transcript, the compression
//! preflight (seam), the budget check and model call, response intake,
//! and then either the tool round or the final answer. [`super::finish`]
//! holds the finalizer.

use std::future::Future;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use pmt_config::{Config, ProfileScope};
use pmt_core::error::Error;
use pmt_core::id::MessageId;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{
    MessageText, Reasoning, StepCall, ToolCall as ToolCallEvent, ToolResult as ToolResultEvent,
    TurnStart, TurnStep, Usage,
};
use pmt_providers::classify::ErrorClass;
use pmt_providers::cost::CallContext;
use pmt_providers::request::{CacheControl, Request};
use pmt_store::{NewMessage, Visibility};
use pmt_tools::Status;

use super::events;
use super::exit::{ExitReason, TurnId, TurnResult, TurnUsage};
use super::interrupt::Issuer;
use super::iteration::IterationBudget;
use super::lease::{self, Refresher};
use super::model::{ModelReply, RouteInfo};
use super::tools::ToolOutcome;
use super::{Agent, TurnInput, TurnOptions, transcript};
use crate::prompt::frozen::Frozen;
use crate::prompt::system_blocks;

/// The window assumed for `@` reference limits when the route has none.
const FALLBACK_WINDOW: u64 = 128_000;

/// How a turn ended, before the finalizer.
#[derive(Debug)]
pub(super) struct Ending {
    pub(super) reason: ExitReason,
    pub(super) final_response: Option<String>,
    pub(super) error: Option<Error>,
    pub(super) issuer: Option<String>,
    /// Run the summary handoff (budget exits, except an unpriced refusal).
    pub(super) handoff: bool,
}

impl Ending {
    pub(super) fn new(reason: ExitReason) -> Self {
        Self {
            reason,
            final_response: None,
            error: None,
            issuer: None,
            handoff: reason.hands_off(),
        }
    }

    pub(super) fn failed(reason: ExitReason, error: Error) -> Self {
        Self {
            error: Some(error),
            ..Self::new(reason)
        }
    }

    fn interrupted(issuer: &Issuer) -> Self {
        let reason = match issuer {
            Issuer::User => ExitReason::InterruptedByUser,
            Issuer::System(_) => ExitReason::InterruptedBySystem,
        };
        Self {
            issuer: Some(issuer.name().to_owned()),
            error: Some(Error::new(
                &pmt_core::error::codes::TURN_INTERRUPTED,
                format!("turn interrupted by {}", issuer.name()),
            )),
            ..Self::new(reason)
        }
    }
}

pub(super) async fn run(
    agent: &Agent,
    scope: &ProfileScope,
    input: TurnInput,
    options: TurnOptions,
) -> Result<TurnResult, Error> {
    let setup = &agent.setup;
    let id = TurnId::new();
    // `@` references are expanded before admission: the admitted message
    // is the expanded one.
    let (input, notices) = expand(agent, input).await;
    let lease = match lease::admit(&setup.store, setup.session, options.lease).await {
        Ok(lease) => lease,
        Err(err) if err.code().code == "turn.lease_unavailable" => {
            let mut turn = Turn::new(agent, scope.config(), id, options);
            turn.notices = notices;
            return turn
                .finish(Ending::failed(ExitReason::LeaseUnavailable, err), None)
                .await;
        }
        Err(err) => return Err(err),
    };
    let refresher = Refresher::start(
        setup.store.clone(),
        lease.clone(),
        options.lease.renew_every,
        options.interrupt.clone(),
    );
    let mut turn = Turn::new(agent, scope.config(), id, options);
    turn.notices = notices;
    let result = turn.run(input).await;
    drop(refresher);
    if let Err(err) = setup.store.release_lease(&lease) {
        tracing::warn!(error = %err, "turn lease could not be released; it lapses at its expiry");
    }
    result
}

/// The input with its `@` references expanded (when the surface offers
/// them) and the warnings for the user.
async fn expand(agent: &Agent, mut input: TurnInput) -> (TurnInput, Vec<String>) {
    let Some(references) = &agent.setup.references else {
        return (input, Vec::new());
    };
    let route = agent.setup.model.route().await;
    let expander = crate::references::Expander {
        places: &references.places,
        context_window: route.context_window.unwrap_or(FALLBACK_WINDOW),
        urls: references.urls.as_deref(),
    };
    let result = expander.expand(&input.text).await;
    input.text = result.text;
    (input, result.warnings)
}

/// The state of one running turn.
pub(super) struct Turn<'a> {
    pub(super) agent: &'a Agent,
    pub(super) config: &'a Config,
    pub(super) id: TurnId,
    pub(super) options: TurnOptions,
    pub(super) started_at: Timestamp,
    deadline: Option<Instant>,
    pub(super) iterations: IterationBudget,
    pub(super) usage: TurnUsage,
    pub(super) appended: Vec<MessageId>,
    pub(super) notices: Vec<String>,
}

impl<'a> Turn<'a> {
    fn new(agent: &'a Agent, config: &'a Config, id: TurnId, options: TurnOptions) -> Self {
        let budget = options
            .run_budget
            .or(config.agent.run_budget_secs.map(Duration::from_secs));
        let max = agent
            .setup
            .max_iterations
            .unwrap_or(config.agent.max_iterations);
        Self {
            agent,
            config,
            id,
            // With no run budget the clock is never read again.
            deadline: budget.map(|b| Instant::now() + b),
            options,
            started_at: Timestamp::now(),
            iterations: IterationBudget::new(max),
            usage: TurnUsage::default(),
            appended: Vec::new(),
            notices: Vec::new(),
        }
    }

    async fn run(mut self, input: TurnInput) -> Result<TurnResult, Error> {
        let setup = &self.agent.setup;
        setup.model.start_turn().await;
        let route = setup.model.route().await;
        let frozen = self.freeze(&route).await?;
        let row = transcript::user_row(&input.text, &input.additions);
        let message_id = row.id;
        let wire = row.content.clone().unwrap_or_default();
        self.append(&row)?;
        self.emit(Event::TurnStart(TurnStart {
            message_id: Some(message_id.to_string()),
        }))
        .await;
        self.emit(Event::UserMessage(MessageText {
            message_id: Some(message_id.to_string()),
            text: wire,
        }))
        .await;
        let ending = self.iterate(&frozen).await?;
        self.finish(ending, Some(&frozen)).await
    }

    async fn iterate(&mut self, frozen: &Frozen) -> Result<Ending, Error> {
        loop {
            if let Some(end) = self.stopped() {
                return Ok(end);
            }
            if !self.iterations.consume() {
                return Ok(Ending::new(ExitReason::IterationLimit));
            }
            // Steer delivery, transcript repair and the compression
            // preflight run here once their owners land (15.8, 15.9, 16.x).
            let route = self.agent.setup.model.route().await;
            let request = self.request(frozen, &route, true)?;
            if let Some(end) = self.check_budget(&request, &route).await? {
                self.iterations.refund(); // nothing was sent
                return Ok(end);
            }
            let reply = match self.call(&request).await {
                Ok(reply) => reply,
                Err(end) => return Ok(end),
            };
            self.record_call(&reply).await?;
            if reply.is_empty() {
                return Ok(Ending::failed(
                    ExitReason::EmptyResponse,
                    Error::new(
                        &pmt_core::error::codes::TURN_EMPTY_RESPONSE,
                        "the model returned an empty response",
                    ),
                ));
            }
            self.keep_reply(&reply).await?;
            if reply.tool_calls.is_empty() {
                let mut end = Ending::new(ExitReason::Completed);
                end.final_response = Some(reply.text);
                return Ok(end);
            }
            if let Some(end) = self.tool_round(&reply).await? {
                return Ok(end);
            }
        }
    }

    /// The interrupt and run-budget check at the top of an iteration.
    fn stopped(&self) -> Option<Ending> {
        if let Some(issuer) = self.options.interrupt.issuer() {
            return Some(Ending::interrupted(&issuer));
        }
        self.deadline
            .is_some_and(|d| Instant::now() >= d)
            .then(|| Ending::new(ExitReason::RunBudgetExhausted))
    }

    /// The request of the next call: the frozen prompt and tools plus the
    /// stored transcript (toolless for the summary handoff).
    pub(super) fn request(
        &self,
        frozen: &Frozen,
        route: &RouteInfo,
        with_tools: bool,
    ) -> Result<Request, Error> {
        let setup = &self.agent.setup;
        let rows = setup.store.messages(setup.session, Visibility::Active)?;
        let mut messages = transcript::wire(&rows);
        if let Some(last) = messages.last_mut() {
            last.cache_breakpoint = true;
        }
        Ok(Request {
            model: route.model.clone(),
            system: system_blocks(&frozen.tiers),
            messages,
            tools: if with_tools {
                frozen.tools.clone()
            } else {
                Vec::new()
            },
            max_output_tokens: None,
            cache: CacheControl::for_session(&self.config.prompt_cache, &setup.session.to_string()),
            reasoning: None,
        })
    }

    /// Sends `request`, racing the interrupt and the run budget.
    pub(super) async fn call(&mut self, request: &Request) -> Result<ModelReply, Ending> {
        let setup = &self.agent.setup;
        let mut ctx = CallContext::new(setup.session, "main");
        ctx.turn_id = Some(self.id.to_string());
        let deltas = self.options.deltas.clone();
        let call = setup.model.call(request, &ctx, &*deltas);
        let failure = match self.race(call).await? {
            Ok(reply) => return Ok(reply),
            Err(failure) => failure,
        };
        for fallback in &failure.fallbacks {
            self.emit(Event::ProviderFallback(fallback.clone())).await;
        }
        let reason = if failure.class == Some(ErrorClass::ContextOverflow) {
            ExitReason::ContextExhausted
        } else {
            ExitReason::ProviderFailed
        };
        Err(Ending::failed(reason, failure.error))
    }

    /// Runs `work` until it finishes, the turn is interrupted or the run
    /// budget runs out.
    async fn race<T>(&self, work: impl Future<Output = T>) -> Result<T, Ending> {
        let token = self.options.interrupt.token().clone();
        let deadline = self.deadline;
        tokio::select! {
            out = work => Ok(out),
            () = token.cancelled() => Err(self.stopped().unwrap_or_else(|| Ending::interrupted(&Issuer::User))),
            () = until(deadline) => Err(Ending::new(ExitReason::RunBudgetExhausted)),
        }
    }

    /// Usage, ledger mirror and fallback events of a finished call.
    pub(super) async fn record_call(&mut self, reply: &ModelReply) -> Result<(), Error> {
        for fallback in &reply.fallbacks {
            self.emit(Event::ProviderFallback(fallback.clone())).await;
        }
        let cost = reply.record.as_ref().and_then(|r| r.cost_usd);
        self.usage.add(&reply.usage, cost);
        if let Some(record) = &reply.record {
            self.emit(Event::ModelCall(record.clone())).await;
        }
        self.after_call().await
    }

    /// Stores the assistant row and emits its events.
    pub(super) async fn keep_reply(&mut self, reply: &ModelReply) -> Result<(), Error> {
        let row = transcript::assistant_row(reply);
        let id = row.id.to_string();
        self.append(&row)?;
        if let Some(text) = &reply.reasoning {
            self.emit(Event::AssistantReasoning(Reasoning {
                message_id: Some(id.clone()),
                text: Some(text.clone()),
                items: None,
            }))
            .await;
        }
        if !reply.text.is_empty() {
            self.emit(Event::AssistantMessage(MessageText {
                message_id: Some(id),
                text: reply.text.clone(),
            }))
            .await;
        }
        Ok(())
    }

    async fn tool_round(&mut self, reply: &ModelReply) -> Result<Option<Ending>, Error> {
        let calls = &reply.tool_calls;
        for call in calls {
            self.emit(Event::ToolCall(ToolCallEvent {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                arguments: serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
            }))
            .await;
        }
        let token = self.options.interrupt.token().child();
        let tools = self.agent.setup.tools.clone();
        let outcomes = match self.race(tools.run(calls, &token)).await {
            Ok(outcomes) => outcomes,
            Err(end) => {
                token.cancel();
                return Ok(Some(end));
            }
        };
        let mut steps = Vec::with_capacity(calls.len());
        for (call, outcome) in calls.iter().zip(&outcomes) {
            self.append(&transcript::tool_row(call, &outcome.result))?;
            self.emit(Event::ToolResult(result_event(call, outcome)))
                .await;
            steps.push(StepCall {
                tool: call.name.clone(),
                args_digest: digest(&call.arguments),
                duration_ms: outcome.duration_ms,
                outcome: outcome.result.status.as_str().to_owned(),
            });
        }
        self.emit(Event::TurnStep(TurnStep {
            iteration: self.iterations.used(),
            calls: steps,
            usage: usage_payload(&self.usage),
        }))
        .await;
        let stop = self.agent.setup.guardrail.after_tools(calls, &outcomes);
        Ok(stop.map(|why| {
            self.notices.push(why);
            Ending::new(ExitReason::GuardrailStop)
        }))
    }

    pub(super) fn append(&mut self, row: &NewMessage) -> Result<(), Error> {
        let setup = &self.agent.setup;
        setup.store.append_message(setup.session, row)?;
        self.appended.push(row.id);
        Ok(())
    }

    pub(super) async fn emit(&self, event: Event) {
        events::emit(&*self.agent.setup.events, self.id.as_str(), event).await;
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

fn result_event(call: &super::model::RequestedCall, outcome: &ToolOutcome) -> ToolResultEvent {
    let ok = matches!(outcome.result.status, Status::Ok | Status::Degraded);
    let text = outcome.result.text_content();
    ToolResultEvent {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        ok,
        duration_ms: Some(outcome.duration_ms),
        output: ok.then(|| text.clone()),
        error: (!ok).then(|| pmt_obs::trajectory::payload::Failure {
            kind: "tool".to_owned(),
            code: Some(format!("tool.{}", outcome.result.status.as_str())),
            message: text,
        }),
    }
}

fn digest(text: &str) -> String {
    crate::sha256(text.as_bytes())
}

pub(super) fn usage_payload(usage: &TurnUsage) -> Usage {
    Usage {
        tokens: pmt_store::Tokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            reasoning: usage.reasoning_tokens,
        }
        .into(),
        cost_usd: usage.cost_usd,
    }
}
