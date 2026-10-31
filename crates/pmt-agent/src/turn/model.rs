//! The turn loop's view of the model: one call in, one collected reply out.
//!
//! [`ModelPort`] keeps the loop independent of how routes, fallbacks and
//! credentials are wired; [`ProviderModel`] is the production port over
//! the provider layer (resolver, fallback chain, [`ModelCaller`]), which
//! also serves the built-in `mock` provider the tests script.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use pmt_config::Config;
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
}

impl ProviderModel {
    /// A port over `caller` starting on `fallback`'s primary route.
    #[must_use]
    pub fn new(setup: ProviderSetup, caller: ModelCaller, fallback: Fallback) -> Self {
        Self {
            setup,
            caller,
            fallback: Mutex::new(fallback),
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
        let route = setup
            .resolver()
            .resolve_target(primary, &mut Trace::default())
            .await?;
        let chain = fallback::chain(&setup.config, &setup.registry)?;
        let state = Fallback::new(route, chain, setup.config.fallback.min_health);
        Ok(Self::new(setup, caller, state))
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
        let called = match self
            .caller
            .call(&mut state, &walk, request, Some(ctx))
            .await
        {
            Ok(called) => called,
            Err(err) => return Err(call_failure(&err)),
        };
        drop(state);
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
        CallError::Refused(_) => ModelFailure {
            error: err.to_error(),
            class: None,
            fallbacks: Vec::new(),
        },
        CallError::Failed(failed) => ModelFailure {
            error: failed.to_error(),
            class: Some(failed.error.class),
            fallbacks: failed.fallbacks.iter().map(Activated::payload).collect(),
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
