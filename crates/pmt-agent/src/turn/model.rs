//! The turn loop's view of the model: one call in, one collected reply out.
//!
//! [`ModelPort`] keeps the loop independent of how routes, fallbacks and
//! credentials are wired; [`ProviderModel`] is the production port over
//! the provider layer (resolver, fallback chain, [`ModelCaller`]), which
//! also serves the built-in `mock` provider the tests script.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::Config;
use pmt_config::schema::{ReasoningLevel, WireFormat};
use pmt_core::error::Error;
use pmt_obs::trajectory::payload::{ModelCall, ProviderFallback};
use pmt_obs::usage::{Price, Prices as _};
use pmt_providers::capability::Needs;
use pmt_providers::classify::ErrorClass;
use pmt_providers::cost::CallContext;
use pmt_providers::event::{StopReason, StreamEvent, Usage};
use pmt_providers::fallback::{self, Activated, Fallback, RouteHealth, Walk};
use pmt_providers::metadata::Catalog;
use pmt_providers::request::Request;
use pmt_providers::resolve::{
    AdHoc, CredentialExtras, EndpointProbe, Resolver, SecretSource, Target, Trace,
};
use pmt_providers::{CallError, ModelCaller, ProviderRegistry, Route};
use tokio::sync::Mutex;

/// Sees the credential of every route a call is sent on (a surface
/// announces a borrowed login's first use with it).
pub type CredentialWatch = Arc<dyn Fn(&pmt_providers::resolve::Credential) + Send + Sync>;

/// A boxed future of a port call.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the loop needs to know about the current route before a call.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteInfo {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Per-million prices; `None` when unpriced.
    pub price: Option<Price>,
    /// Context window in tokens, when known.
    pub context_window: Option<u64>,
    /// The model's output cap, when known (reserved from the window).
    pub max_output_tokens: Option<u64>,
    /// The route's wire format.
    pub wire_format: WireFormat,
    /// The route's `billing_source` (spec: model-providers, Billing source
    /// transparency).
    pub billing_source: String,
    /// The route compacts context server-side (spec: context-compression,
    /// Server-side Responses compaction).
    pub server_compaction: bool,
    /// A local inference server (memory reviews may wait for an idle
    /// machine).
    pub local: bool,
    /// The route has a fast mode priority tier.
    pub fast_tier: bool,
}

/// One tool call the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedCall {
    /// Call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as JSON text.
    pub arguments: String,
}

/// A complete model response.
#[derive(Debug, Clone, Default)]
pub struct ModelReply {
    /// Answer text.
    pub text: String,
    /// Readable reasoning, when the model returned any.
    pub reasoning: Option<String>,
    /// Requested tool calls, in order.
    pub tool_calls: Vec<RequestedCall>,
    /// Token usage.
    pub usage: Usage,
    /// Why the response ended.
    pub stop: Option<StopReason>,
    /// The `model.call` record (ledger row mirror), when one was written.
    pub record: Option<ModelCall>,
    /// Fallback activations on the way (`provider.fallback` events).
    pub fallbacks: Vec<ProviderFallback>,
    /// The server-side compaction item the response returned.
    pub compaction_item: Option<serde_json::Value>,
}

impl ModelReply {
    /// Neither text nor tool calls.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.tool_calls.is_empty()
    }
}

/// A call that produced no usable reply.
#[derive(Debug)]
pub struct ModelFailure {
    /// The user-facing error.
    pub error: Error,
    /// The provider's error class, when the provider answered.
    pub class: Option<ErrorClass>,
    /// Fallback activations tried.
    pub fallbacks: Vec<ProviderFallback>,
    /// The provider's `Retry-After`.
    pub retry_after: Option<Duration>,
    /// Answer text reached the user before the failure.
    pub streamed: bool,
}

impl ModelFailure {
    /// A failure of `class` that streamed nothing.
    #[must_use]
    pub fn new(error: pmt_core::Error, class: Option<ErrorClass>) -> Self {
        Self {
            error,
            class,
            fallbacks: Vec::new(),
            retry_after: None,
            streamed: false,
        }
    }
}

/// Receives answer text as it streams (the surface prints it).
pub trait DeltaSink: Send + Sync {
    /// More answer text.
    fn text(&self, _text: &str) {}
}

/// Discards deltas.
#[derive(Debug, Default)]
pub struct NoDeltas;

impl DeltaSink for NoDeltas {}

/// The model as the turn loop calls it.
pub trait ModelPort: Send + Sync {
    /// The route the next call goes to.
    fn route(&self) -> BoxFuture<'_, RouteInfo>;

    /// Turn start: a fallback route whose primary cooled down is dropped.
    fn start_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Sends `request` and collects the reply.
    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>>;
}

/// Everything [`ProviderModel`] resolves fallback routes with.
pub struct ProviderSetup {
    /// The profile config.
    pub config: Arc<Config>,
    /// The profile's providers.
    pub registry: Arc<ProviderRegistry>,
    /// The profile's secrets.
    pub secrets: Arc<dyn SecretSource>,
    /// Pools and sign-ins (`pmt_providers::auth::Accounts`).
    pub extras: Arc<dyn CredentialExtras>,
    /// Local endpoint reachability.
    pub probe: Arc<dyn EndpointProbe>,
    /// Model metadata and prices.
    pub catalog: Arc<Catalog>,
    /// Benches and health of candidate routes.
    pub health: Arc<dyn RouteHealth>,
    /// The launch-only endpoint, if any.
    pub ad_hoc: Option<AdHoc>,
    /// Asks before a fallback from a subscription to a metered credential
    /// (`providers.<id>.confirm_metered_fallback`); `None` refuses such a
    /// move (non-interactive sessions).
    pub metered_confirm: Option<Arc<dyn pmt_providers::fallback::MeteredConfirm>>,
}

impl ProviderSetup {
    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            registry: &self.registry,
            config: &self.config,
            secrets: &*self.secrets,
            extras: &*self.extras,
            probe: &*self.probe,
            ad_hoc: self.ad_hoc.clone(),
        }
    }
}

/// The production port: the session's primary route, its fallback chain
/// and the caller that records every call in the ledger.
pub struct ProviderModel {
    setup: ProviderSetup,
    caller: ModelCaller,
    fallback: Mutex<Fallback>,
    /// The session's `/reasoning` level (`None`: the config decides).
    session_reasoning: std::sync::Mutex<Option<ReasoningLevel>>,
    watch: std::sync::Mutex<Option<CredentialWatch>>,
    /// Parameters set on every request body (a delegated child's
    /// `delegation.request_overrides`).
    body_overrides: std::sync::Mutex<Option<Arc<serde_json::Map<String, serde_json::Value>>>>,
}

impl ProviderModel {
    /// A port over `caller` starting on `fallback`'s primary route; each
    /// route gets at most `agent.api_max_retries` attempts per call.
    #[must_use]
    pub fn new(setup: ProviderSetup, caller: ModelCaller, fallback: Fallback) -> Self {
        let caller = caller.with_attempt_cap(setup.config.agent.api_max_retries);
        Self {
            setup,
            caller,
            fallback: Mutex::new(fallback),
            session_reasoning: std::sync::Mutex::default(),
            watch: std::sync::Mutex::default(),
            body_overrides: std::sync::Mutex::default(),
        }
    }

    /// Sets `overrides` on every later request body, on every route of
    /// the chain (spec: subagent-delegation, Child request overrides and
    /// direct endpoints).
    pub fn set_body_overrides(
        &self,
        overrides: Option<Arc<serde_json::Map<String, serde_json::Value>>>,
    ) {
        *self
            .body_overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = overrides;
    }

    /// Calls `watch` with the credential of each route a call is sent on.
    pub fn watch_credentials(&self, watch: CredentialWatch) {
        *self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(watch);
    }

    /// Sets the session's reasoning level from the next request (`None`
    /// returns to the config's level).
    pub fn set_reasoning(&self, level: Option<ReasoningLevel>) {
        *self
            .session_reasoning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = level;
    }

    /// The session's reasoning level.
    #[must_use]
    pub fn reasoning(&self) -> Option<ReasoningLevel> {
        *self
            .session_reasoning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `request` carrying the effort for `route` (spec: model-providers,
    /// Reasoning effort defaults, overrides and display): the session
    /// level, else the config's, clamped to what the route's model
    /// supports. A bad override was refused by config validation, so it
    /// sends no control here rather than failing the call.
    fn with_effort(&self, request: &Request, route: &Route) -> Request {
        let (meta, _) =
            self.setup
                .catalog
                .metadata_at(&route.provider, &route.model, Some(&route.base_url));
        let effort = pmt_providers::reasoning::effort(
            &self.setup.config.agent,
            self.reasoning(),
            &route.model,
            &meta,
        );
        Request {
            reasoning: effort.ok().and_then(|e| e.sent),
            body_overrides: self
                .body_overrides
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            ..request.clone()
        }
    }

    /// Resolves the session's `primary` route and the configured fallback
    /// chain.
    ///
    /// # Errors
    ///
    /// Resolution failures of the primary (`provider.*`, `model.*`) and of
    /// the chain entries' providers.
    pub async fn start(
        setup: ProviderSetup,
        caller: ModelCaller,
        primary: &Target,
    ) -> Result<Self, Error> {
        let chain = fallback::chain(&setup.config, &setup.registry)?;
        Self::start_with_chain(setup, caller, primary, chain).await
    }

    /// Resolves the session's `primary` route with its own fallback
    /// `chain` (see `fallback::scoped_chain` for pinned owners).
    ///
    /// # Errors
    ///
    /// Resolution failures of the primary.
    pub async fn start_with_chain(
        setup: ProviderSetup,
        caller: ModelCaller,
        primary: &Target,
        chain: Vec<Target>,
    ) -> Result<Self, Error> {
        let route = setup
            .resolver()
            .resolve_target(primary, &mut Trace::default())
            .await?;
        let mut state = Fallback::new(route, chain, setup.config.fallback.min_health);
        if let Some(confirm) = &setup.metered_confirm {
            state = state.with_metered_confirm(Arc::clone(confirm));
        }
        Ok(Self::new(setup, caller, state))
    }

    /// Like [`ProviderModel::start`] with no fallback chain: a delegated
    /// child that pinned its own model fails on that model instead of
    /// moving to the parent's chain (spec: subagent-delegation, Child model
    /// routing).
    ///
    /// # Errors
    ///
    /// Resolution errors of `primary`.
    pub async fn start_pinned(
        setup: ProviderSetup,
        caller: ModelCaller,
        primary: &Target,
    ) -> Result<Self, Error> {
        Self::start_with_chain(setup, caller, primary, Vec::new()).await
    }

    fn info(&self, route: &Route) -> RouteInfo {
        let (meta, _) =
            self.setup
                .catalog
                .metadata_at(&route.provider, &route.model, Some(&route.base_url));
        RouteInfo {
            provider: route.provider.clone(),
            model: route.model.clone(),
            price: self.setup.catalog.price(&route.provider, &route.model),
            context_window: Some(meta.context_window),
            max_output_tokens: meta.max_output_tokens,
            wire_format: route.wire_format,
            billing_source: route.billing_source(),
            server_compaction: route.supports_server_compaction,
            local: route.local,
            fast_tier: route.fast_tier.is_some(),
        }
    }

    async fn send(
        &self,
        request: &Request,
        ctx: &CallContext,
        deltas: &dyn DeltaSink,
    ) -> Result<ModelReply, ModelFailure> {
        let setup = &self.setup;
        let resolver = setup.resolver();
        let walk = Walk {
            resolver: &resolver,
            health: &*setup.health,
            catalog: &setup.catalog,
            needs: Needs::of(request),
        };
        let started = Instant::now();
        let mut state = self.fallback.lock().await;
        let request = &self.with_effort(request, state.current());
        let called = match self
            .caller
            .call(&mut state, &walk, request, Some(ctx))
            .await
        {
            Ok(called) => called,
            Err(err) => return Err(call_failure(&err)),
        };
        drop(state);
        let watch = self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(watch) = watch {
            watch(&called.route.credential);
        }
        let fallbacks = called.fallbacks.iter().map(Activated::payload).collect();
        let mut stream = called.stream;
        let mut reply = ModelReply {
            fallbacks,
            ..ModelReply::default()
        };
        let mut reasoning = String::new();
        while let Some(event) = stream.next().await {
            intake(&mut reply, &mut reasoning, event, deltas);
        }
        if let Some(failure) = stream.failure() {
            return Err(ModelFailure {
                error: failure.to_route_error(&called.route),
                class: Some(failure.class),
                fallbacks: reply.fallbacks,
                retry_after: failure.retry_after,
                streamed: !reply.text.is_empty(),
            });
        }
        reply.reasoning = (!reasoning.is_empty()).then_some(reasoning);
        let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        reply.record = stream
            .recorded()
            .and_then(|r| r.as_ref().ok())
            .map(|event| model_call(event, latency));
        Ok(reply)
    }
}

impl ModelPort for ProviderModel {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let state = self.fallback.lock().await;
            self.info(state.current())
        })
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.fallback
                .lock()
                .await
                .start_turn(jiff::Timestamp::now());
        })
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        Box::pin(self.send(request, ctx, deltas))
    }
}

fn call_failure(err: &CallError) -> ModelFailure {
    match err {
        CallError::Refused(_) => ModelFailure::new(err.to_error(), None),
        CallError::Failed(failed) => ModelFailure {
            fallbacks: failed.fallbacks.iter().map(Activated::payload).collect(),
            retry_after: failed.error.retry_after,
            ..ModelFailure::new(failed.to_error(), Some(failed.error.class))
        },
    }
}

/// Folds one stream event into the reply.
fn intake(
    reply: &mut ModelReply,
    reasoning: &mut String,
    event: StreamEvent,
    deltas: &dyn DeltaSink,
) {
    match event {
        StreamEvent::TextDelta { text } => {
            deltas.text(&text);
            reply.text.push_str(&text);
        }
        StreamEvent::ReasoningDelta { text } => reasoning.push_str(&text),
        StreamEvent::ToolCallEnd {
            id,
            name,
            arguments,
            ..
        } => reply.tool_calls.push(RequestedCall {
            id,
            name,
            arguments,
        }),
        StreamEvent::Usage { usage } => reply.usage = usage,
        StreamEvent::CompactionItem { item } => reply.compaction_item = Some(item),
        StreamEvent::Stop { stop_reason } => reply.stop = Some(stop_reason),
        StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => {}
    }
}

fn model_call(event: &pmt_store::UsageEvent, latency_ms: u64) -> ModelCall {
    ModelCall {
        call_id: event.call_id.clone(),
        provider: event.provider.clone(),
        model: event.model.clone(),
        purpose: event.purpose.clone(),
        request_hash: event.request_hash.clone().unwrap_or_default(),
        cache_hit: event.cache_hit,
        latency_ms,
        tokens: event.tokens.into(),
        cost_usd: event.cost_usd,
        cost_status: event.cost_status.as_str().to_owned(),
        billing_source: event.billing_source.clone(),
    }
}
