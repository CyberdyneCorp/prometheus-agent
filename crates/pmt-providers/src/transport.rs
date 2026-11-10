//! One HTTP attempt of a model call: encode for the route's wire format,
//! authenticate for its credential, stream the response through the
//! decoder and normaliser, and classify every failure (spec:
//! model-providers, Error classification; request and stream idle
//! timeouts).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::schema::WireFormat;
use pmt_core::id::SessionId;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use tokio::time::{Instant, timeout_at};

use crate::classify::{self, ErrorClass, ProviderError};
use crate::cost::Pending;
use crate::descriptor::BundledWire;
use crate::event::{Normalizer, StopReason, StreamEvent, Usage};
use crate::mock::{self, Feed, Mock};
use crate::request::Request;
use crate::resolve::{BoxFuture, Route};
use crate::sse::SseParser;
use crate::wire::{self, Decode, EncodeOptions};

/// Per-request time limits (`providers.<id>.request_timeout_secs`,
/// `stream_idle_timeout_secs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// The whole request, first byte to last event.
    pub request: Duration,
    /// Longest gap between stream events.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_mins(10),
            idle: Duration::from_mins(2),
        }
    }
}

/// The JSON body `request` is sent as on `route` (also what fixture tests
/// compare): the route's model, and a prompt cache key only where the
/// provider supports one and caching is on.
#[must_use]
pub fn body(route: &Route, request: &Request) -> serde_json::Value {
    let mut request = request.clone();
    request.model.clone_from(&route.model);
    let cache_key = match route.wire_format {
        WireFormat::AnthropicMessages => None,
        _ if request.cache.ttl == crate::request::CacheTtl::Off => None,
        _ if route.supports_prompt_cache => request.cache.key.as_deref(),
        _ => None,
    };
    let compaction = request
        .server_compaction
        .as_ref()
        .filter(|_| route.supports_server_compaction);
    let options = EncodeOptions {
        cache_key,
        compaction,
    };
    let mut body = match route.bundled {
        Some(bundled) => wire::encode_bundled(bundled, &request),
        None => wire::encode(route.wire_format, &request, &options),
    };
    if let Some(extra) = &route.body_extra {
        crate::request::apply_overrides(&mut body, extra);
    }
    if let (Some(speed), Some(tier)) = (request.speed, route.fast_tier) {
        tier.apply(&mut body, speed);
    }
    if let Some(overrides) = &request.body_overrides {
        crate::request::apply_overrides(&mut body, overrides);
    }
    body
}

/// The conversations of a profile's sessions (spec: model-providers,
/// Session affinity headers and per-model wire routing). Each open session
/// is bound to its conversation id (its root session id, or the kanban
/// task id of a kanban worker), and a call carries the affinity of the
/// session its [`CallContext`](crate::cost::CallContext) names. Keyed per session rather than held
/// per caller because one daemon process shares a profile's callers
/// between all of its concurrent sessions.
#[derive(Debug, Clone, Default)]
pub struct Conversations(Arc<Mutex<HashMap<SessionId, Bound>>>);

/// A session's conversation id and how many open handles bound it.
#[derive(Debug)]
struct Bound {
    conversation: String,
    handles: usize,
}

impl Conversations {
    /// Calls of `session` belong to `conversation` while the returned
    /// [`Binding`] lives.
    #[must_use]
    pub fn bind(&self, session: SessionId, conversation: &str) -> Binding {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let bound = map.entry(session).or_insert_with(|| Bound {
            conversation: String::new(),
            handles: 0,
        });
        conversation.clone_into(&mut bound.conversation);
        bound.handles += 1;
        Binding {
            conversations: self.clone(),
            session,
        }
    }

    /// The opaque affinity value of `session`'s calls: a stable hash of
    /// its conversation id; `None` for a session that is not bound.
    #[must_use]
    pub fn affinity(&self, session: SessionId) -> Option<String> {
        use sha2::{Digest, Sha256};
        let map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let digest = Sha256::digest(map.get(&session)?.conversation.as_bytes());
        Some(crate::request::hex(&digest[..16]))
    }
}

/// Keeps a session bound to its conversation; dropping the last binding
/// of a session forgets it, so a long-lived daemon does not accumulate
/// closed sessions.
#[derive(Debug)]
pub struct Binding {
    conversations: Conversations,
    session: SessionId,
}

impl Drop for Binding {
    fn drop(&mut self) {
        let mut map = self
            .conversations
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(bound) = map.get_mut(&self.session) {
            bound.handles -= 1;
            if bound.handles == 0 {
                map.remove(&self.session);
            }
        }
    }
}

/// The headers a request on `route` carries besides its credential: the
/// `affinity` header when the route's provider routes by one, and the
/// beta flags its body needs.
pub fn route_headers(
    route: &Route,
    request: &Request,
    affinity: Option<String>,
) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let (Some(name), Some(value)) = (&route.affinity_header, affinity) {
        headers.push((name.clone(), value));
    }
    let beta = request
        .speed
        .and(route.fast_tier)
        .and_then(crate::fast::FastTier::beta);
    if let Some(beta) = beta {
        headers.push(("anthropic-beta".to_owned(), beta.to_owned()));
    }
    headers
}

/// Request hooks of model providers registered by plugins (spec:
/// plugin-system, Model-provider plugins): header building and request
/// preparation run in the plugin, the HTTP request and streaming stay in
/// the host, so credentials never reach the plugin.
pub trait RequestHooks: Send + Sync {
    /// Whether `provider` has hooks.
    fn handles(&self, provider: &str) -> bool;
    /// Headers added to every request to `provider` (the credential
    /// header is set after them and wins).
    fn headers<'a>(
        &'a self,
        provider: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, String>>;
    /// The JSON body about to be sent to `provider`, rewritten.
    fn prepare<'a>(
        &'a self,
        provider: &'a str,
        body: serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value, String>>;
}

/// Sends model requests: over HTTP, or to the in-process [`Mock`] for
/// routes of the built-in `mock` provider.
#[derive(Clone)]
pub(crate) struct Sender {
    client: reqwest::Client,
    mock: Arc<Mock>,
    calls: Arc<crate::calls::Calls>,
    hooks: Option<Arc<dyn RequestHooks>>,
    pii: Option<Arc<pmt_core::pii::Pii>>,
    conversations: Conversations,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl Sender {
    /// HTTP over `client`; mock routes echo until [`with_mock`](Self::with_mock).
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            mock: Arc::new(Mock::echo()),
            calls: Arc::default(),
            hooks: None,
            pii: None,
            conversations: Conversations::default(),
        }
    }

    /// Tags every request of a bound session with its conversation's
    /// affinity header.
    pub(crate) fn with_conversations(mut self, conversations: Conversations) -> Self {
        self.conversations = conversations;
        self
    }

    /// Replaces PII in every request before it leaves the process.
    pub(crate) fn with_pii(mut self, pii: Arc<pmt_core::pii::Pii>) -> Self {
        self.pii = Some(pii);
        self
    }

    /// Runs plugin request hooks of the providers they handle.
    pub(crate) fn with_request_hooks(mut self, hooks: Arc<dyn RequestHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Serves mock routes from `mock` (the session's script).
    pub(crate) fn with_mock(mut self, mock: Arc<Mock>) -> Self {
        self.mock = mock;
        self
    }

    /// Runs `calls` around every call (replay, cache, recording).
    pub(crate) fn with_calls(mut self, calls: Arc<crate::calls::Calls>) -> Self {
        self.calls = calls;
        self
    }

    /// The layers around every call.
    pub(crate) fn calls(&self) -> &crate::calls::Calls {
        &self.calls
    }

    /// The layers, shared.
    pub(crate) fn calls_handle(&self) -> Arc<crate::calls::Calls> {
        Arc::clone(&self.calls)
    }

    /// Sends one attempt of `session`'s call and waits for its first
    /// normalised event.
    ///
    /// # Errors
    ///
    /// The classified failure when nothing was streamed yet (HTTP error,
    /// connection failure, timeout, an in-stream error before any event).
    pub(crate) async fn send(
        &self,
        route: &Route,
        request: &Request,
        session: Option<SessionId>,
    ) -> Result<ModelStream, ProviderError> {
        let redacted = self.pii.as_deref().map(|pii| request.redacted(pii));
        let request = redacted.as_ref().unwrap_or(request);
        if mock::serves(route) {
            let feed = self.mock.reply(route, request)?;
            return ModelStream::start(Source::Mock(feed), Limits::of(route)).await;
        }
        if let Some(launch) = crate::external::launch(route) {
            let limits = Limits::of(route);
            let feed = crate::external::AcpFeed::start(launch, request, limits).await?;
            return ModelStream::start(Source::Acp(Box::new(feed)), limits).await;
        }
        let hooks = self.hooks.as_deref().filter(|h| h.handles(&route.provider));
        let affinity = session.and_then(|s| self.conversations.affinity(s));
        let mut extra = route_headers(route, request, affinity);
        // Never sent to a provider unless `observability.otel.propagate_external`.
        if let Some(traceparent) =
            session.and_then(|s| pmt_obs::otel::context::external_traceparent(&s.to_string()))
        {
            extra.push(("traceparent".to_owned(), traceparent));
        }
        send_http(&self.client, hooks, route, request, extra).await
    }
}

/// The body field carrying the route's opaque credential handle to the
/// plugin's `prepare-request` (spec: model-providers, Plugin OAuth and
/// external-process providers): it names the credential
/// (`subscription:<provider>/<label>`) and never holds a token, verifier
/// or `state`; it is removed before the request is sent.
pub const CREDENTIAL_HANDLE: &str = "prometheus_credential";

/// The body and extra headers of a request, through the plugin's hooks.
/// A hook that fails (a crashed or stalled plugin) fails the call without
/// retrying the same route, and the fallback chain applies (spec:
/// python-bridge, Model-provider plugins through the bridge).
async fn hooked(
    hooks: &dyn RequestHooks,
    route: &Route,
    body: serde_json::Value,
) -> Result<(serde_json::Value, Vec<(String, String)>), ProviderError> {
    let failed = |what: &str, why: String| ProviderError {
        class: crate::classify::ErrorClass::ServerError,
        status: None,
        message: format!(
            "provider plugin of '{}' failed to {what}: {why}",
            route.provider
        ),
        retry_after: None,
        reset_at: None,
        refusal: None,
        body: None,
        verdict: Some(Box::new(crate::classify::Reclassified {
            reason: "provider_plugin_failed".to_owned(),
            retryable: Some(false),
            should_compress: false,
            should_rotate_credential: false,
            should_fallback: Some(true),
            message: None,
        })),
    };
    let mut body = body;
    if let Some(fields) = body.as_object_mut() {
        fields.insert(
            CREDENTIAL_HANDLE.to_owned(),
            route.credential.describe().into(),
        );
    }
    let mut body = hooks
        .prepare(&route.provider, body)
        .await
        .map_err(|why| failed("prepare the request", why))?;
    if let Some(fields) = body.as_object_mut() {
        fields.remove(CREDENTIAL_HANDLE);
    }
    let headers = hooks
        .headers(&route.provider)
        .await
        .map_err(|why| failed("build the headers", why))?;
    Ok((body, headers))
}

async fn send_http(
    client: &reqwest::Client,
    hooks: Option<&dyn RequestHooks>,
    route: &Route,
    request: &Request,
    extra: Vec<(String, String)>,
) -> Result<ModelStream, ProviderError> {
    let limits = Limits::of(route);
    let (body, mut headers) = match hooks {
        Some(hooks) => hooked(hooks, route, body(route, request)).await?,
        None => (body(route, request), Vec::new()),
    };
    headers.extend(extra);
    let body = serde_json::to_vec(&body).expect("JSON values serialize");
    let url = match route.bundled {
        Some(bundled) => wire::bundled_endpoint(bundled, &route.base_url, &route.model),
        None => wire::endpoint(route.wire_format, &route.base_url),
    };
    if route.bundled == Some(BundledWire::BedrockConverse) {
        headers.extend(bedrock_signature(route, &url, &body)?);
    }
    let mut builder = client
        .post(url)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .header("accept", accept(route))
        .body(body);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    if route.bundled != Some(BundledWire::BedrockConverse) {
        builder = authenticate(builder, route);
    }
    let response = match timeout_at(limits.deadline, builder.send()).await {
        Err(_) => return Err(timed_out("no response within the request timeout")),
        Ok(Err(err)) => return Err(transport_error(&err)),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let text = timeout_at(limits.deadline, response.text())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        return Err(classify::from_response(
            status.as_u16(),
            &headers,
            &text,
            Timestamp::now(),
        ));
    }
    let body = HttpBody {
        response: Some(response),
        framing: Framing::of(route),
        decoder: match route.bundled {
            Some(bundled) => wire::bundled_decoder(bundled),
            None => wire::decoder(route.wire_format),
        },
    };
    ModelStream::start(Source::Http(body), limits).await
}

/// The `accept` header: Bedrock streams AWS event-stream frames.
fn accept(route: &Route) -> &'static str {
    if route.bundled == Some(BundledWire::BedrockConverse) {
        "application/vnd.amazon.eventstream"
    } else {
        "text/event-stream"
    }
}

/// The `SigV4` headers of a Bedrock request, signed with the AWS keys the
/// credential chain minted for the route.
fn bedrock_signature(
    route: &Route,
    url: &str,
    body: &[u8],
) -> Result<Vec<(String, String)>, ProviderError> {
    let keys = route
        .credential
        .value()
        .and_then(crate::aws::Keys::from_secret)
        .ok_or_else(|| ProviderError::new(ErrorClass::Auth, "no AWS credentials for the route"))?;
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ProviderError::new(ErrorClass::Config, format!("bad Bedrock URL: {e}")))?;
    let region = route
        .region
        .as_deref()
        .unwrap_or(crate::aws::DEFAULT_REGION);
    Ok(crate::aws::sigv4::sign(
        &crate::aws::sigv4::Signing {
            method: "POST",
            url: &parsed,
            headers: &[],
            body,
            region,
            service: crate::aws::SERVICE,
            now: Timestamp::now(),
        },
        &keys,
    ))
}

/// How a response body is split into events.
#[derive(Debug)]
enum Framing {
    /// Server-Sent Events (every format but Bedrock's).
    Sse(SseParser),
    /// AWS event-stream frames.
    Aws(crate::aws::eventstream::Parser),
}

impl Framing {
    fn of(route: &Route) -> Self {
        if route.bundled == Some(BundledWire::BedrockConverse) {
            Self::Aws(crate::aws::eventstream::Parser::default())
        } else {
            Self::Sse(SseParser::default())
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<crate::sse::SseEvent>, ProviderError> {
        match self {
            Self::Sse(parser) => Ok(parser.feed(bytes)),
            Self::Aws(parser) => parser.feed(bytes).map_err(|why| wire::malformed(&why)),
        }
    }

    fn finish(&mut self) -> Option<crate::sse::SseEvent> {
        match self {
            Self::Sse(parser) => parser.finish(),
            // A partial frame at the end is a truncated stream.
            Self::Aws(_) => None,
        }
    }
}

/// Adds the route's credential: in its provider's key header
/// (`x-goog-api-key`, `api-key`, Anthropic's `x-api-key`), else as a
/// bearer token, which sign-ins and `key_header = "authorization"` routes
/// always use.
pub(crate) fn authenticate(
    builder: reqwest::RequestBuilder,
    route: &Route,
) -> reqwest::RequestBuilder {
    let anthropic = route.wire_format == WireFormat::AnthropicMessages && route.bundled.is_none();
    let builder = if anthropic {
        builder.header("anthropic-version", wire::ANTHROPIC_VERSION)
    } else {
        builder
    };
    let Some(secret) = route.credential.value() else {
        return builder;
    };
    let key_header = match route.key_header.as_deref() {
        _ if route.credential.is_bearer() => None,
        Some(name) if name.eq_ignore_ascii_case("authorization") => None,
        Some(name) => Some(name),
        None if anthropic => Some("x-api-key"),
        None => None,
    };
    let sensitive = |text: &str| {
        let mut value =
            HeaderValue::from_str(text).unwrap_or_else(|_| HeaderValue::from_static(""));
        value.set_sensitive(true);
        value
    };
    if let Some(name) = key_header {
        return builder.header(name, sensitive(secret.expose()));
    }
    let builder = builder.header(
        reqwest::header::AUTHORIZATION,
        sensitive(&format!("Bearer {}", secret.expose())),
    );
    subscription_identity(builder, route)
}

/// What a subscription request must carry besides its bearer token
/// (spec: model-providers, Anthropic Claude Max sign-in; OpenAI Codex
/// ChatGPT plan sign-in): Anthropic's OAuth beta flag, and the ChatGPT
/// account the token belongs to.
fn subscription_identity(
    mut builder: reqwest::RequestBuilder,
    route: &Route,
) -> reqwest::RequestBuilder {
    if !route.credential.is_subscription() {
        return builder;
    }
    if route.wire_format == WireFormat::AnthropicMessages {
        builder = builder.header("anthropic-beta", wire::ANTHROPIC_OAUTH_BETA);
    }
    if let Some(account) = route.credential.account_id() {
        builder = builder.header("ChatGPT-Account-ID", account);
    }
    builder
}

/// A timeout failure (the class every request or idle timeout gets).
pub(crate) fn timed_out(message: &str) -> ProviderError {
    ProviderError::new(ErrorClass::Timeout, message)
}

fn transport_error(err: &reqwest::Error) -> ProviderError {
    let class = if err.is_timeout() {
        ErrorClass::Timeout
    } else {
        ErrorClass::Network
    };
    // reqwest errors carry the URL, never headers, so no credential leaks.
    ProviderError::new(class, err.to_string())
}

/// The time limits of one attempt: the request deadline and the longest
/// gap between stream events.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    deadline: Instant,
    idle: Duration,
}

impl Limits {
    /// The limits of an attempt on `route` starting now.
    pub(crate) fn of(route: &Route) -> Self {
        Self {
            deadline: Instant::now() + route.timeouts.request,
            idle: route.timeouts.idle,
        }
    }

    /// Awaits the next piece of the stream within the idle and request
    /// deadlines.
    pub(crate) async fn within<F: Future>(self, next: F) -> Result<F::Output, ProviderError> {
        let idle_deadline = (Instant::now() + self.idle).min(self.deadline);
        match timeout_at(idle_deadline, next).await {
            Err(_) if idle_deadline < self.deadline => {
                Err(timed_out("no stream event within the idle timeout"))
            }
            Err(_) => Err(timed_out(
                "the response did not finish within the request timeout",
            )),
            Ok(output) => Ok(output),
        }
    }
}

/// Where a stream's events come from.
#[derive(Debug)]
enum Source {
    Http(HttpBody),
    Mock(Feed),
    Acp(Box<crate::external::AcpFeed>),
    /// A served response: one recorded chunk per read, then its recorded
    /// failure, if it failed.
    Served(VecDeque<Vec<StreamEvent>>, Option<ProviderError>),
}

/// An HTTP response body decoded from its wire format's SSE events.
#[derive(Debug)]
struct HttpBody {
    response: Option<reqwest::Response>,
    framing: Framing,
    decoder: Box<dyn Decode>,
}

impl HttpBody {
    /// Reads and decodes the next body chunk, or finishes at its end.
    async fn step(&mut self, out: &mut Normalizer, limits: Limits) -> Result<(), ProviderError> {
        let Some(response) = self.response.as_mut() else {
            return Err(wire::truncated());
        };
        let chunk = match limits.within(response.chunk()).await? {
            Err(err) => return Err(transport_error(&err)),
            Ok(chunk) => chunk,
        };
        let Some(bytes) = chunk else {
            self.response = None;
            if let Some(event) = self.framing.finish() {
                self.decoder.event(&event, out)?;
            }
            if !out.is_finished() {
                self.decoder.end(out)?;
            }
            return Ok(());
        };
        for event in self.framing.feed(&bytes)? {
            self.decoder.event(&event, out)?;
        }
        Ok(())
    }
}

/// A started response: normalised events in order, ending with exactly
/// one `stop` (`stop_reason = "error"` when the stream failed after it
/// started; [`failure`](Self::failure) then says why).
#[derive(Debug)]
pub struct ModelStream {
    source: Source,
    normalizer: Normalizer,
    pending: VecDeque<StreamEvent>,
    limits: Limits,
    failure: Option<ProviderError>,
    usage: Option<Usage>,
    ledger: Option<Pending>,
    recorded: Option<Result<pmt_store::UsageEvent, pmt_core::Error>>,
    on_end: Vec<OnEnd>,
    tap: Option<Tap>,
    /// The response was served, not sent (a cache hit).
    served: bool,
    capture: Option<Capture>,
}

/// What [`Capture`] calls with a successful response.
type CapturedFn = dyn FnOnce(crate::calls::Recorded) + Send + Sync;

/// Collects a response's chunks and hands them over once it ended
/// successfully (a failed response is dropped).
pub(crate) struct Capture {
    chunks: Vec<Vec<StreamEvent>>,
    done: Box<CapturedFn>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}

/// Sees every event of a stream as it is read (a MoA trace collects the
/// aggregator's output with it).
pub(crate) struct Tap(pub(crate) Box<dyn FnMut(&StreamEvent) + Send + Sync>);

impl std::fmt::Debug for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tap")
    }
}

/// What [`OnEnd`] calls.
type EndFn = dyn FnOnce(Option<&ProviderError>, Option<Usage>) + Send + Sync;

/// Called once when a stream ends: its failure, if any, and its usage.
pub(crate) struct OnEnd(Box<EndFn>);

impl OnEnd {
    pub(crate) fn new(
        f: impl FnOnce(Option<&ProviderError>, Option<Usage>) + Send + Sync + 'static,
    ) -> Self {
        Self(Box::new(f))
    }
}

impl std::fmt::Debug for OnEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnEnd")
    }
}

impl std::fmt::Debug for dyn Decode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Decode")
    }
}

impl ModelStream {
    /// A stream over `source` that has produced its first event.
    async fn start(source: Source, limits: Limits) -> Result<Self, ProviderError> {
        let mut stream = Self {
            source,
            normalizer: Normalizer::default(),
            pending: VecDeque::new(),
            limits,
            failure: None,
            usage: None,
            ledger: None,
            recorded: None,
            on_end: Vec::new(),
            tap: None,
            served: false,
            capture: None,
        };
        stream.fill().await?;
        Ok(stream)
    }

    /// A stream replaying `recorded` chunk by chunk (a served call).
    pub(crate) async fn served(
        recorded: crate::calls::Recorded,
        limits: Limits,
    ) -> Result<Self, ProviderError> {
        let failure = recorded
            .failure
            .as_ref()
            .map(crate::calls::Failure::to_error);
        let source = Source::Served(recorded.chunks.into_iter().collect(), failure);
        let mut stream = Self::start(source, limits).await?;
        stream.served = true;
        Ok(stream)
    }

    /// Whether the response was served rather than sent.
    #[must_use]
    pub fn is_served(&self) -> bool {
        self.served
    }

    /// Hands the response to `done` once it ended.
    pub(crate) fn capture(
        &mut self,
        done: impl FnOnce(crate::calls::Recorded) + Send + Sync + 'static,
    ) {
        let first: Vec<StreamEvent> = self.pending.iter().cloned().collect();
        self.capture = Some(Capture {
            chunks: if first.is_empty() {
                Vec::new()
            } else {
                vec![first]
            },
            done: Box::new(done),
        });
    }

    /// The stream ended with `stop`: the capture gets the response, with
    /// its failure when it failed.
    fn ended(&mut self, event: &StreamEvent) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        let failure = match (&self.failure, event) {
            (Some(error), _) => Some(crate::calls::Failure::of(error)),
            (
                None,
                StreamEvent::Stop {
                    stop_reason: StopReason::Error,
                },
            ) => Some(crate::calls::Failure::of(&wire::truncated())),
            _ => None,
        };
        (capture.done)(crate::calls::Recorded {
            chunks: capture.chunks,
            usage: self.usage.unwrap_or_default(),
            failure,
        });
    }

    /// The next event; `None` after `stop`.
    pub async fn next(&mut self) -> Option<StreamEvent> {
        if self.pending.is_empty()
            && !self.normalizer.is_finished()
            && let Err(error) = self.fill().await
        {
            self.fail(error);
        }
        let event = self.pending.pop_front()?;
        if let Some(Tap(tap)) = &mut self.tap {
            tap(&event);
        }
        if matches!(event, StreamEvent::Stop { .. }) {
            for OnEnd(on_end) in std::mem::take(&mut self.on_end) {
                on_end(self.failure.as_ref(), self.usage);
            }
            self.ended(&event);
        }
        if let StreamEvent::Usage { usage } = &event {
            self.usage = Some(*usage);
            if let Some(pending) = self.ledger.take() {
                let recorded = pending.record(usage, self.served);
                if let Err(err) = &recorded {
                    tracing::error!(
                        code = err.code().code,
                        "usage ledger write failed: {}",
                        err.message()
                    );
                }
                self.recorded = Some(recorded);
            }
        }
        Some(event)
    }

    /// Calls `on_end` once, when the stream's `stop` event is read (after
    /// any added before it).
    pub(crate) fn on_end(&mut self, on_end: OnEnd) {
        self.on_end.push(on_end);
    }

    /// Shows `tap` every event from now on.
    pub(crate) fn tap(&mut self, tap: Tap) {
        self.tap = Some(tap);
    }

    /// Records the call's ledger row when its usage arrives.
    pub(crate) fn record_with(&mut self, pending: Pending) {
        self.ledger = Some(pending);
    }

    /// The ledger row written for this call (after its `usage` event), or
    /// the ledger error.
    #[must_use]
    pub fn recorded(&self) -> Option<&Result<pmt_store::UsageEvent, pmt_core::Error>> {
        self.recorded.as_ref()
    }

    /// Every remaining event.
    pub async fn collect(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.next().await {
            events.push(event);
        }
        events
    }

    /// Why the stream ended with `stop_reason = "error"`.
    #[must_use]
    pub fn failure(&self) -> Option<&ProviderError> {
        self.failure.as_ref()
    }

    /// The usage event, once it has been read.
    #[must_use]
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Reads until at least one event is pending or the stream finished.
    async fn fill(&mut self) -> Result<(), ProviderError> {
        while self.pending.is_empty() && !self.normalizer.is_finished() {
            match &mut self.source {
                Source::Http(body) => body.step(&mut self.normalizer, self.limits).await?,
                Source::Mock(feed) => feed.step(&mut self.normalizer, self.limits).await?,
                Source::Acp(feed) => feed.step(&mut self.normalizer, self.limits).await?,
                Source::Served(chunks, failure) => match chunks.pop_front() {
                    Some(chunk) => self.normalizer.replay(chunk),
                    None => return Err(failure.take().unwrap_or_else(wire::truncated)),
                },
            }
            let drained: Vec<StreamEvent> = self.normalizer.drain().collect();
            if let Some(capture) = &mut self.capture
                && !drained.is_empty()
            {
                capture.chunks.push(drained.clone());
            }
            self.pending.extend(drained);
        }
        Ok(())
    }

    fn fail(&mut self, error: ProviderError) {
        if let Source::Http(body) = &mut self.source {
            body.response = None;
        }
        self.failure = Some(error);
        self.normalizer.finish(Some(StopReason::Error));
        self.pending.extend(self.normalizer.drain());
    }
}
