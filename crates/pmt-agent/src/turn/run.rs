//! One turn, phase by phase (spec: agent-turn-loop, Iteration phases).
//!
//! Each iteration runs, in order: the interrupt and budget check, steer
//! delivery, request assembly from the frozen prompt and the repaired
//! transcript, the compression preflight ([`super::compression`]), the cache-prefix
//! check, the budget check and the model call, response intake, and then
//! either the tool round ([`super::round`]) or the final answer.
//! [`super::finish`] holds the finalizer.

use std::future::Future;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::{Config, ProfileScope};
use pmt_core::error::Error;
use pmt_core::id::MessageId;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{MessageText, Reasoning, TurnStart, Usage};
use pmt_providers::request::{CacheControl, Request};
use pmt_store::{NewMessage, Role, Visibility};
use tokio::time::Instant;

use super::events;
use super::exit::{ExitReason, TurnId, TurnResult, TurnUsage};
use super::guardrails::LoopGuard;
use super::guards;
use super::interrupt::Issuer;
use super::iteration::IterationBudget;
use super::lease::{self, Refresher};
use super::model::{ModelReply, RouteInfo};
use super::{Agent, TurnInput, TurnOptions, breakpoints, repair, transcript};
use crate::compress::Trigger;
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

/// What the loop does after a reply.
pub(super) enum Next {
    /// Another iteration.
    Continue,
    /// The turn ends.
    End(Ending),
}

/// What sending a request led to.
enum Sent {
    Reply(Box<ModelReply>),
    /// Compacted after an overflow: send again.
    Retry,
    End(Ending),
}

/// The wall-clock run budget of a turn (on the runtime clock).
#[derive(Debug, Clone, Copy)]
pub(super) struct RunClock {
    pub(super) started: Instant,
    pub(super) budget: Duration,
}

impl RunClock {
    pub(super) fn deadline(self) -> Instant {
        self.started + self.budget
    }

    /// Whether 80% of the budget has elapsed.
    pub(super) fn wrapping_up(self) -> bool {
        self.started.elapsed() >= self.budget.mul_f64(0.8)
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
    /// `None` without a run budget: the clock is then never read.
    pub(super) clock: Option<RunClock>,
    pub(super) iterations: IterationBudget,
    pub(super) usage: TurnUsage,
    pub(super) appended: Vec<MessageId>,
    /// The role of the newest row this turn stored.
    pub(super) last_role: Option<Role>,
    pub(super) notices: Vec<String>,
    pub(super) guard: LoopGuard,
    /// Empty replies retried in the current streak.
    empty_streak: u32,
    /// Narration nudges sent this turn.
    nudges: u32,
    /// The run-budget wrap-up notice was sent.
    pub(super) wrap_up_sent: bool,
    /// The label of the next compression check.
    pub(super) next_trigger: Trigger,
    /// Compaction attempts this turn.
    pub(super) compression_attempts: u32,
    /// Active rows the latest request was built from.
    pub(super) request_rows: usize,
    /// This turn's recall block (wire only).
    pub(super) ephemeral: Option<super::recall::Ephemeral>,
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
            clock: budget.map(|budget| RunClock {
                started: Instant::now(),
                budget,
            }),
            options,
            started_at: Timestamp::now(),
            iterations: IterationBudget::new(max),
            usage: TurnUsage::default(),
            appended: Vec::new(),
            last_role: None,
            notices: Vec::new(),
            guard: LoopGuard::new(&config.tool_loop_guardrails, agent.setup.interactive),
            empty_streak: 0,
            nudges: 0,
            wrap_up_sent: false,
            next_trigger: Trigger::TurnStart,
            compression_attempts: 0,
            request_rows: 0,
            ephemeral: None,
        }
    }

    async fn run(mut self, input: TurnInput) -> Result<TurnResult, Error> {
        let setup = &self.agent.setup;
        setup.model.start_turn().await;
        let route = setup.model.route().await;
        let mut frozen = self.freeze(&route).await?;
        let text = match self.agent.steers.take() {
            Some(steer) => format!("{steer}\n\n{}", input.text),
            None => input.text,
        };
        let row = transcript::user_row(&text, &input.additions, &input.images);
        let message_id = row.id;
        let wire = row.content.clone().unwrap_or_default();
        self.append(&row)?;
        self.emit(Event::TurnStart(TurnStart {
            message_id: Some(message_id.to_string()),
        }))
        .await;
        self.emit(Event::UserMessage(MessageText {
            message_id: Some(message_id.to_string()),
            text: wire.clone(),
        }))
        .await;
        self.recall(&text, &wire).await;
        let ending = self.iterate(&mut frozen).await?;
        self.finish(ending, Some(&frozen)).await
    }

    async fn iterate(&mut self, frozen: &mut Frozen) -> Result<Ending, Error> {
        loop {
            if let Some(end) = self.stopped() {
                return Ok(end);
            }
            if !self.iterations.consume() {
                return Ok(Ending::new(ExitReason::IterationLimit));
            }
            self.deliver_steers()?;
            let route = self.agent.setup.model.route().await;
            let estimate = self.request(frozen, &route, true)?.estimated_tokens();
            self.preflight(frozen, &route, estimate).await?;
            let request = self.request(frozen, &route, true)?;
            if let Some(end) = self.check_budget(&request, &route).await? {
                self.iterations.refund(); // nothing was sent
                return Ok(end);
            }
            self.check_prefix(&request).await;
            let reply = match self.send(frozen, &request, &route).await? {
                Sent::Reply(reply) => *reply,
                Sent::Retry => continue,
                Sent::End(end) => return Ok(end),
            };
            self.record_call(&reply).await?;
            let tool_round = !reply.tool_calls.is_empty();
            if let Next::End(end) = self.intake(reply, &request, &route).await? {
                return Ok(end);
            }
            if tool_round {
                self.next_trigger = Trigger::PostTool;
            }
        }
    }

    /// Sends `request`; an overflow compacts and asks for the same
    /// request again (its iteration refunded).
    async fn send(
        &mut self,
        frozen: &mut Frozen,
        request: &Request,
        route: &RouteInfo,
    ) -> Result<Sent, Error> {
        match self.call(request).await {
            Ok(reply) => Ok(Sent::Reply(Box::new(reply))),
            Err(end) if end.reason == ExitReason::ContextExhausted => {
                Ok(match self.overflow(frozen, route).await? {
                    None => {
                        self.iterations.refund();
                        Sent::Retry
                    }
                    Some(end) => Sent::End(end),
                })
            }
            Err(end) => Ok(Sent::End(end)),
        }
    }

    /// Response intake: the empty-response and narration guards, then the
    /// final answer or the tool round.
    async fn intake(
        &mut self,
        reply: ModelReply,
        request: &Request,
        route: &RouteInfo,
    ) -> Result<Next, Error> {
        if reply.is_empty() {
            self.emit_step(Vec::new()).await;
            let agent = &self.config.agent;
            if guards::retry_empty(agent, request, route, &reply, self.empty_streak) {
                self.empty_streak += 1;
                return Ok(Next::Continue);
            }
            return Ok(Next::End(Ending::failed(
                ExitReason::EmptyResponse,
                Error::new(
                    &pmt_core::error::codes::TURN_EMPTY_RESPONSE,
                    "the model returned an empty response",
                ),
            )));
        }
        self.empty_streak = 0;
        self.keep_reply(&reply).await?;
        if !reply.tool_calls.is_empty() {
            return Ok(match self.tool_round(&reply).await? {
                Some(end) => Next::End(end),
                None => Next::Continue,
            });
        }
        self.emit_step(Vec::new()).await;
        if self.nudges < guards::MAX_NUDGES
            && guards::continuation_applies(&self.config.agent, route)
            && guards::narrates(&reply, !request.tools.is_empty())
        {
            self.nudges += 1;
            self.append(&transcript::notice_row(
                guards::NUDGE,
                "intent_continuation",
            ))?;
            return Ok(Next::Continue);
        }
        let mut end = Ending::new(ExitReason::Completed);
        end.final_response = Some(reply.text);
        Ok(Next::End(end))
    }

    /// The interrupt and run-budget check at the top of an iteration.
    pub(super) fn stopped(&self) -> Option<Ending> {
        if let Some(issuer) = self.options.interrupt.issuer() {
            return Some(Ending::interrupted(&issuer));
        }
        self.clock
            .is_some_and(|c| Instant::now() >= c.deadline())
            .then(|| Ending::new(ExitReason::RunBudgetExhausted))
    }

    /// The request of the next call: the frozen prompt and tools plus the
    /// repaired transcript (toolless for the summary handoff).
    pub(super) fn request(
        &mut self,
        frozen: &Frozen,
        route: &RouteInfo,
        with_tools: bool,
    ) -> Result<Request, Error> {
        let setup = &self.agent.setup;
        let rows = setup.store.messages(setup.session, Visibility::Active)?;
        self.request_rows = rows.len();
        let mut cache =
            CacheControl::for_session(&self.config.prompt_cache, &setup.session.to_string());
        cache.ttl = self.agent.cache_ttl(&self.config.prompt_cache, route);
        let mut request = Request {
            model: route.model.clone(),
            system: system_blocks(&frozen.tiers),
            messages: repair::wire_with(&rows, &|row| image_parts(&setup.store, row)),
            tools: if with_tools {
                frozen.tools.clone()
            } else {
                Vec::new()
            },
            max_output_tokens: None,
            cache,
            reasoning: None,
        };
        if let Some(ephemeral) = &mut self.ephemeral {
            ephemeral.attach(&mut request.messages);
        }
        breakpoints::plan(&mut request, route.wire_format);
        Ok(request)
    }

    /// Runs a model call until it finishes, any stop arrives (a soft stop
    /// too: the call has no side effects) or the run budget runs out.
    pub(super) async fn race_model<T>(&self, work: impl Future<Output = T>) -> Result<T, Ending> {
        let token = self.options.interrupt.any().clone();
        self.race(work, token).await
    }

    /// Runs a tool batch until it finishes, a hard stop arrives or the run
    /// budget runs out; a soft stop lets it finish.
    pub(super) async fn race_tools<T>(&self, work: impl Future<Output = T>) -> Result<T, Ending> {
        let token = self.options.interrupt.token().clone();
        self.race(work, token).await
    }

    async fn race<T>(
        &self,
        work: impl Future<Output = T>,
        token: pmt_core::cancel::CancelToken,
    ) -> Result<T, Ending> {
        let deadline = self.clock.map(RunClock::deadline);
        // A stop wins over work finishing at the same moment.
        tokio::select! {
            biased;
            () = token.cancelled() => Err(self.stopped().unwrap_or_else(|| Ending::interrupted(&Issuer::User))),
            () = until(deadline) => Err(Ending::new(ExitReason::RunBudgetExhausted)),
            out = work => Ok(out),
        }
    }

    /// Usage, ledger mirror and fallback events of a finished call.
    pub(super) async fn record_call(&mut self, reply: &ModelReply) -> Result<(), Error> {
        for fallback in &reply.fallbacks {
            self.emit(Event::ProviderFallback(fallback.clone())).await;
        }
        let cost = reply.record.as_ref().and_then(|r| r.cost_usd);
        self.usage.add(&reply.usage, cost);
        self.observe(reply)?;
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

    pub(super) fn append(&mut self, row: &NewMessage) -> Result<(), Error> {
        let setup = &self.agent.setup;
        setup.store.append_message(setup.session, row)?;
        self.appended.push(row.id);
        self.last_role = Some(row.role);
        Ok(())
    }

    pub(super) async fn emit(&self, event: Event) {
        events::emit(&*self.agent.setup.events, self.id.as_str(), event).await;
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
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

/// The images `row` stored, as request parts (pixels or descriptions). An image whose
/// blob is gone becomes a note, so it cannot fail the request.
fn image_parts(
    store: &pmt_store::Store,
    row: &pmt_store::Message,
) -> Vec<pmt_providers::request::Part> {
    use pmt_providers::request::Part;
    use pmt_tools::vision::routing::{Inline, inline};
    transcript::images_of(row)
        .into_iter()
        .map(|image| {
            if let Some(description) = &image.description {
                return Part::Text {
                    text: format!("[image {}: {description}]", image.name),
                };
            }
            let bytes = pmt_store::BlobRef::parse(&image.blob)
                .ok_or_else(|| "bad blob key".to_owned())
                .and_then(|blob| store.get_blob(&blob).map_err(|e| e.to_string()));
            let data = bytes.and_then(pmt_tools::vision::ImageData::sniff);
            match data.map(|data| inline(data, &image.name)) {
                Ok(Inline::Image(image)) => Part::Image {
                    data: image.base64(),
                    media_type: image.mime,
                    is_url: false,
                },
                Ok(Inline::Note(note)) => Part::Text { text: note },
                Err(why) => Part::Text {
                    text: format!("[image {} is unavailable: {why}]", image.name),
                },
            }
        })
        .collect()
}
