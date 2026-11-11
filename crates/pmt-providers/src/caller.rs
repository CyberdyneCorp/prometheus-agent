//! Making a model call (spec: model-providers, Error classification and
//! same-route retries; In-turn fallback activation; agent-turn-loop, Model
//! call retries): retries on the same route with jittered backoff, then
//! the fallback chain, recording one ledger row per call that reports
//! usage.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_obs::usage::UsageRecorder;

use crate::calls::Before;
use crate::capability;
use crate::classify::{ErrorClass, ProviderError};
use crate::cost::{CallContext, Pending};
use crate::fallback::{Activated, Fallback, Walk};
use crate::metadata::Catalog;
use crate::mock::Mock;
use crate::request::Request;
use crate::resolve::{BoxFuture, Route};
use crate::transport::{ModelStream, Sender};

/// Per-request credential work (spec: model-providers, Credential pool
/// selection strategies; Exhausted credential cooldowns; Token refresh and
/// dead-token quarantine; Borrowed CLI logins), implemented by
/// [`crate::auth::Accounts`].
pub trait CredentialHooks: Send + Sync {
    /// The route one request goes out on: its pool key picked, its sign-in
    /// token re-read (refreshed when about to expire), the request
    /// counted. An error refuses the request unsent (a dead sign-in, every
    /// credential benched).
    fn prepare<'a>(&'a self, route: &'a Route) -> BoxFuture<'a, Result<Route, ProviderError>>;

    /// After `route`'s request failed with `error`: benches its credential
    /// or, when `may_recover` and a sign-in token was refused, refreshes
    /// it.
    fn failed<'a>(
        &'a self,
        route: &'a Route,
        error: &'a ProviderError,
        may_recover: bool,
    ) -> BoxFuture<'a, Recovery>;
}

/// A failed provider call, as `transform_api_error_classification`
/// hooks see it (spec: plugin-system).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailureInfo {
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The HTTP status, when there was a response.
    pub status_code: Option<u16>,
    /// The built-in class (`rate_limit`, `config`...).
    pub error_type: String,
    /// The provider's error code (`error.code`/`error.type` of the body).
    pub error_code: Option<String>,
    /// The provider's message.
    pub error_message: String,
    /// The raw response body.
    pub error_body: Option<String>,
    /// Estimated request tokens.
    pub approx_tokens: u64,
    /// The route's context window, when known.
    pub context_length: Option<u64>,
    /// Messages in the request.
    pub message_count: usize,
}

/// Classification hooks (spec: plugin-system,
/// `transform_api_error_classification`): consulted once per failed call
/// before the retry and fallback policy applies; the first valid decision
/// wins and `None` keeps the built-in class.
pub trait ErrorClassifier: Send + Sync {
    /// The decision about `failure`, with the class of its reason.
    fn classify<'a>(
        &'a self,
        failure: &'a FailureInfo,
    ) -> BoxFuture<'a, Option<(crate::classify::Reclassified, crate::classify::ErrorClass)>>;
}

/// One call's outcome, as health scoring sees it (spec: model-providers,
/// Provider health scoring).
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    /// `None` on success, else the failure's class.
    pub class: Option<ErrorClass>,
    /// From sending to the first streamed event.
    pub ttft: Option<std::time::Duration>,
    /// Output tokens per second after the first event.
    pub output_tps: Option<f64>,
}

/// Sees the outcome of every model call sent (refusals are not sent and
/// are not seen).
pub trait CallObserver: Send + Sync {
    /// `route`'s call ended with `outcome`.
    fn observed(&self, route: &Route, outcome: &Observed);
}

/// What a failed request's credential work decided.
#[derive(Debug)]
pub enum Recovery {
    /// Nothing changes; the usual retry policy applies.
    Continue,
    /// The credential was refreshed; send again now.
    RetryNow,
    /// Fail with this error instead (the sign-in is dead).
    Fail(ProviderError),
}

/// Sends `request` on `route` once, running `hooks` around it: prepared,
/// and sent once more at once when a refused sign-in token was refreshed.
/// The sender's call layers come first: a replay or a cache hit serves the
/// response without sending (and without credentials), and a successful
/// sent response is cached and recorded. Returns the stream and the route
/// it went out on.
///
/// # Errors
///
/// The classified failure, the hooks' refusal, or a replay's refusal.
pub(crate) async fn send_once(
    sender: &Sender,
    hooks: Option<&dyn CredentialHooks>,
    route: &Route,
    request: &Request,
    session: Option<SessionId>,
) -> Result<(ModelStream, Route), ProviderError> {
    let calls = sender.calls();
    if calls.is_empty() {
        return send_prepared(sender, hooks, route, request, session).await;
    }
    let key = crate::calls::CallKey::of(route, request);
    let (mut stream, sent_on, served) = match calls.before(&key).await {
        Before::Refuse(error) => return Err(error),
        Before::Serve(recorded) => match &recorded.failure {
            // A recorded failure before anything streamed.
            Some(failure) if recorded.chunks.is_empty() => return Err(failure.to_error()),
            _ => {
                let limits = crate::transport::Limits::of(route);
                (
                    ModelStream::served(recorded, limits).await?,
                    route.clone(),
                    true,
                )
            }
        },
        Before::Send => match send_prepared(sender, hooks, route, request, session).await {
            Ok((stream, prepared)) => (stream, prepared, false),
            Err(error) => {
                // A request refused unsent got no response to record.
                if error.refusal.is_none() {
                    calls.failed(session, &key, &error);
                }
                return Err(error);
            }
        },
    };
    let calls = sender.calls_handle();
    stream.capture(move |recorded| calls.after(session, &key, &recorded, served));
    Ok((stream, sent_on))
}

/// [`send_once`] without the call layers.
async fn send_prepared(
    sender: &Sender,
    hooks: Option<&dyn CredentialHooks>,
    route: &Route,
    request: &Request,
    session: Option<SessionId>,
) -> Result<(ModelStream, Route), ProviderError> {
    let Some(hooks) = hooks else {
        return Ok((sender.send(route, request, session).await?, route.clone()));
    };
    let mut may_recover = true;
    loop {
        let prepared = hooks.prepare(route).await?;
        let error = match sender.send(&prepared, request, session).await {
            Ok(stream) => return Ok((stream, prepared)),
            Err(error) => error,
        };
        match hooks.failed(&prepared, &error, may_recover).await {
            Recovery::RetryNow if may_recover => may_recover = false,
            Recovery::Fail(error) => return Err(error),
            Recovery::RetryNow | Recovery::Continue => return Err(error),
        }
    }
}

/// Sends model requests for one session.
#[derive(Clone)]
pub struct ModelCaller {
    sender: Sender,
    attempt_cap: Option<u32>,
    recorder: Option<Arc<UsageRecorder>>,
    hooks: Option<Arc<dyn CredentialHooks>>,
    classifier: Option<Arc<dyn ErrorClassifier>>,
    observer: Option<Arc<dyn CallObserver>>,
    mixture: Arc<crate::moa::Mixture>,
}

impl std::fmt::Debug for ModelCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelCaller")
            .field("attempt_cap", &self.attempt_cap)
            .field("hooks", &self.hooks.is_some())
            .finish_non_exhaustive()
    }
}

/// A started call.
#[derive(Debug)]
pub struct Called {
    /// The response stream.
    pub stream: ModelStream,
    /// The route that answered.
    pub route: Route,
    /// Fallback activations on the way (each one a `provider.fallback`).
    pub fallbacks: Vec<Activated>,
    /// The advisor blocks a MoA call shows (privacy-filtered), when its
    /// references ran.
    pub advice: Vec<String>,
}

/// A call that failed on every route it could use.
#[derive(Debug)]
pub struct Failed {
    /// The first route's failure (what the turn loop surfaces).
    pub error: ProviderError,
    /// That route.
    pub route: Route,
    /// Fallback activations that were tried.
    pub fallbacks: Vec<Activated>,
}

impl Failed {
    /// The user-facing error.
    #[must_use]
    pub fn to_error(&self) -> Error {
        self.error.to_route_error(&self.route)
    }
}

/// Why [`ModelCaller::call`] produced no stream.
#[derive(Debug)]
pub enum CallError {
    /// The current route's model cannot take the request
    /// (`model.capability_missing`); nothing was sent.
    Refused(Error),
    /// Every usable route failed.
    Failed(Box<Failed>),
}

impl CallError {
    /// The user-facing error.
    #[must_use]
    pub fn to_error(&self) -> Error {
        match self {
            Self::Refused(err) => {
                let copy = Error::new(err.code(), err.message());
                match err.hint() {
                    Some(hint) => copy.with_hint(hint),
                    None => copy,
                }
            }
            Self::Failed(failed) => failed.to_error(),
        }
    }
}

impl ModelCaller {
    /// A caller over `client` (from `pmt_net::client_builder`).
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            sender: Sender::new(client),
            attempt_cap: None,
            recorder: None,
            hooks: None,
            classifier: None,
            observer: None,
            mixture: Arc::default(),
        }
    }

    /// Keeps the profile's Mixture of Agents state in `mixture` (traces,
    /// known secrets, per-session cadence).
    #[must_use]
    pub fn with_mixture(mut self, mixture: Arc<crate::moa::Mixture>) -> Self {
        self.mixture = mixture;
        self
    }

    /// The profile's Mixture of Agents state.
    pub(crate) fn mixture(&self) -> &crate::moa::Mixture {
        &self.mixture
    }

    /// Tells `observer` the outcome of every call sent.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn CallObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Reports a failed attempt to the metrics catalog and the observer.
    fn observe_failure(&self, route: &Route, error: &ProviderError) {
        pmt_obs::metrics::record_model_failure(&route.provider, &route.model, error.class.as_str());
        if let Some(observer) = &self.observer {
            observer.observed(
                route,
                &Observed {
                    class: Some(error.class),
                    ttft: None,
                    output_tps: None,
                },
            );
        }
    }

    /// Reports `stream`'s time to first token and a failure mid-stream to
    /// the metrics catalog, and its outcome to the observer when it ends.
    fn observe_stream(&self, stream: &mut ModelStream, route: &Route, sent: std::time::Instant) {
        let ttft = sent.elapsed();
        if !stream.is_served() {
            pmt_obs::metrics::global().observe(
                pmt_obs::metrics::MODEL_TTFT_MS,
                &[("provider", &route.provider), ("model", &route.model)],
                ttft.as_secs_f64() * 1000.0,
            );
        }
        let (provider, model) = (route.provider.clone(), route.model.clone());
        stream.on_end(crate::transport::OnEnd::new(move |failure, _| {
            if let Some(failure) = failure {
                pmt_obs::metrics::record_model_failure(&provider, &model, failure.class.as_str());
            }
        }));
        let Some(observer) = self.observer.clone() else {
            return;
        };
        let first = std::time::Instant::now();
        let route = route.clone();
        stream.on_end(crate::transport::OnEnd::new(move |failure, usage| {
            let secs = first.elapsed().as_secs_f64();
            #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
            let output_tps = usage
                .filter(|u| u.output_tokens > 0 && secs > 0.0)
                .map(|u| u.output_tokens as f64 / secs);
            observer.observed(
                &route,
                &Observed {
                    class: failure.map(|f| f.class),
                    ttft: Some(ttft),
                    output_tps,
                },
            );
        }));
    }

    /// Consults `classifier` about every failed call before the retry and
    /// fallback policy.
    #[must_use]
    pub fn with_classifier(mut self, classifier: Arc<dyn ErrorClassifier>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Runs `hooks` around requests to the plugin model providers they
    /// handle.
    #[must_use]
    pub fn with_request_hooks(mut self, hooks: Arc<dyn crate::transport::RequestHooks>) -> Self {
        self.sender = self.sender.with_request_hooks(hooks);
        self
    }

    /// `error` after the classification hooks.
    async fn reclassify(
        &self,
        route: &Route,
        request: &Request,
        context_length: Option<u64>,
        error: ProviderError,
    ) -> ProviderError {
        let Some(classifier) = &self.classifier else {
            return error;
        };
        let info = FailureInfo {
            provider: route.provider.clone(),
            model: route.model.clone(),
            status_code: error.status,
            error_type: error.class.as_str().to_owned(),
            error_code: error.body.as_deref().and_then(error_code),
            error_message: error.message.clone(),
            error_body: error.body.clone(),
            approx_tokens: request.estimated_tokens(),
            context_length,
            message_count: request.messages.len(),
        };
        match classifier.classify(&info).await {
            Some((verdict, class)) => error.reclassified(verdict, class),
            None => error,
        }
    }

    /// Runs `hooks` around every request (pools, benches, refresh).
    #[must_use]
    pub fn with_credentials(mut self, hooks: Arc<dyn CredentialHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Tags every request of a session bound in `conversations` with its
    /// conversation's affinity header on routes whose provider routes by
    /// one.
    #[must_use]
    pub fn with_conversations(mut self, conversations: crate::transport::Conversations) -> Self {
        self.sender = self.sender.with_conversations(conversations);
        self
    }

    /// Replaces PII in every request (`privacy.redact_pii`), the mock's
    /// included, before it is sent.
    #[must_use]
    pub fn with_pii(mut self, pii: Arc<pmt_core::pii::Pii>) -> Self {
        self.sender = self.sender.with_pii(pii);
        self
    }

    /// Serves routes of the built-in `mock` provider from `mock` (the
    /// session's script; without one they echo).
    #[must_use]
    pub fn with_mock(mut self, mock: Arc<Mock>) -> Self {
        self.sender = self.sender.with_mock(mock);
        self
    }

    /// Runs `calls` around every call: a replay source, the model-call
    /// cache and the session recorder.
    #[must_use]
    pub fn with_calls(mut self, calls: Arc<crate::calls::Calls>) -> Self {
        self.sender = self.sender.with_calls(calls);
        self
    }

    /// Caps attempts per route (`agent.api_max_retries`, the turn loop's
    /// total attempts) below `providers.<id>.max_retries + 1`.
    #[must_use]
    pub fn with_attempt_cap(mut self, attempts: u32) -> Self {
        self.attempt_cap = Some(attempts.max(1));
        self
    }

    /// Records every call's usage in the ledger.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<UsageRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Sends `request` on `route`, retrying retryable failures that
    /// happen before anything was streamed. With credential hooks each
    /// attempt is prepared (pool key, token), and a refused sign-in token
    /// is refreshed and sent again at once; a request the hooks refuse
    /// (dead sign-in, every credential benched) is not retried.
    ///
    /// # Errors
    ///
    /// The last attempt's classified failure, or the hooks' refusal.
    pub async fn send(
        &self,
        route: &Route,
        request: &Request,
        ctx: Option<&CallContext>,
    ) -> Result<ModelStream, ProviderError> {
        self.send_sized(route, request, ctx, None).await
    }

    /// [`Self::send`], telling classification hooks the route's context
    /// window.
    pub(crate) async fn send_sized(
        &self,
        route: &Route,
        request: &Request,
        ctx: Option<&CallContext>,
        context_length: Option<u64>,
    ) -> Result<ModelStream, ProviderError> {
        let capped = ctx
            .and_then(|c| c.stale_cap.as_ref())
            .map(|cap| cap.apply(route));
        let route = capped.as_ref().unwrap_or(route);
        let mut retries = route.retry;
        if let Some(cap) = self.attempt_cap {
            retries.max_retries = retries.max_retries.min(cap - 1);
        }
        let mut retry = 0;
        loop {
            let sent = std::time::Instant::now();
            let session = ctx.map(|c| c.session_id);
            let sent_once = send_once(&self.sender, self.hooks.as_deref(), route, request, session);
            let error = match sent_once.await {
                Ok((mut stream, prepared)) => {
                    if let (Some(recorder), Some(ctx)) = (&self.recorder, ctx) {
                        stream.record_with(Pending::new(recorder.clone(), &prepared, request, ctx));
                    }
                    self.observe_stream(&mut stream, &prepared, sent);
                    return Ok(stream);
                }
                Err(error) if error.refusal.is_some() => return Err(error),
                Err(error) => {
                    let error = self.reclassify(route, request, context_length, error).await;
                    self.observe_failure(route, &error);
                    error
                }
            };
            let Some(delay) = retries.delay(retry, &error) else {
                return Err(error);
            };
            tracing::debug!(provider = %route.provider, class = error.class.as_str(), ?delay, "retrying model call");
            tokio::time::sleep(delay).await;
            retry += 1;
        }
    }

    /// Sends `request` on the session's current route and, when it fails
    /// with a fallback-eligible class after its retries, on the next
    /// usable fallback entries.
    ///
    /// # Errors
    ///
    /// [`CallError::Refused`] when the current route's model lacks a
    /// capability the request needs; [`CallError::Failed`] with the first
    /// route's error when the class is not fallback-eligible or the chain
    /// is spent.
    pub async fn call(
        &self,
        state: &mut Fallback,
        walk: &Walk<'_>,
        request: &Request,
        ctx: Option<&CallContext>,
    ) -> Result<Called, CallError> {
        let current = state.current();
        let acting = current.acting();
        let (meta, _) =
            walk.catalog
                .metadata_at(&acting.provider, &acting.model, Some(&acting.base_url));
        capability::check(&current.label(), &meta, &walk.needs).map_err(CallError::Refused)?;
        let mut fallbacks = Vec::new();
        let mut first: Option<(ProviderError, Route)> = None;
        loop {
            let route = state.current().clone();
            match self.send_route(&route, request, ctx, walk).await {
                Ok((stream, advice)) => {
                    state.primary_succeeded();
                    return Ok(Called {
                        stream,
                        route,
                        fallbacks,
                        advice,
                    });
                }
                Err(error) => {
                    let activated = state.activate(&error, walk, Timestamp::now()).await;
                    let (activated, (error, route)) = match activated {
                        // The move was refused: that refusal is the turn's error.
                        Err(refused) => (None, (refused, route)),
                        Ok(activated) => (activated, first.get_or_insert((error, route)).clone()),
                    };
                    match activated {
                        Some(activated) => {
                            tracing::warn!(from = %route.label(), to = %format!("{}/{}", activated.to.provider, activated.to.model), reason = activated.reason.as_str(), "provider fallback");
                            fallbacks.push(activated);
                        }
                        None => {
                            return Err(CallError::Failed(Box::new(Failed {
                                error,
                                route,
                                fallbacks,
                            })));
                        }
                    }
                }
            }
        }
    }
}

impl ModelCaller {
    /// One send on `route`: a MoA preset runs its references and
    /// aggregator, any other route takes the request as it is.
    async fn send_route(
        &self,
        route: &Route,
        request: &Request,
        ctx: Option<&CallContext>,
        walk: &Walk<'_>,
    ) -> Result<(ModelStream, Vec<String>), ProviderError> {
        if crate::moa::serves(route) {
            return self.send_moa(route, request, ctx, walk).await;
        }
        let fitted = fit_reasoning(request, route, walk.catalog);
        let (meta, _) =
            walk.catalog
                .metadata_at(&route.provider, &route.model, Some(&route.base_url));
        let stream = self
            .send_sized(
                route,
                fitted.as_ref().unwrap_or(request),
                ctx,
                Some(meta.context_window),
            )
            .await?;
        Ok((stream, Vec::new()))
    }
}

/// The provider's error code in a JSON error body (`error.code`, else
/// `error.type`).
fn error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let code = error.get("code").or_else(|| error.get("type"))?;
    Some(match code {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

/// `request` with its reasoning level clamped to what `route`'s model
/// supports, when that differs (a fallback route may reason less, or not
/// at all); `None` when the request fits as it is.
pub(crate) fn fit_reasoning(
    request: &Request,
    route: &Route,
    catalog: &Catalog,
) -> Option<Request> {
    let level = request.reasoning?;
    let (meta, _) = catalog.metadata_at(&route.provider, &route.model, Some(&route.base_url));
    let (sent, _) = crate::reasoning::clamp(level, &meta);
    (sent != request.reasoning).then(|| Request {
        reasoning: sent,
        ..request.clone()
    })
}
