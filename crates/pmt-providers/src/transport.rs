//! One HTTP attempt of a model call: encode for the route's wire format,
//! authenticate for its credential, stream the response through the
//! decoder and normaliser, and classify every failure (spec:
//! model-providers, Error classification; request and stream idle
//! timeouts).

use std::collections::VecDeque;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::schema::WireFormat;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use tokio::time::{Instant, timeout_at};

use crate::classify::{self, ErrorClass, ProviderError};
use crate::cost::Pending;
use crate::event::{Normalizer, StopReason, StreamEvent, Usage};
use crate::request::Request;
use crate::resolve::Route;
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
    wire::encode(route.wire_format, &request, &EncodeOptions { cache_key })
}

/// Sends one attempt and waits for its first normalised event.
///
/// # Errors
///
/// The classified failure when nothing was streamed yet (HTTP error,
/// connection failure, timeout, an in-stream error before any event).
pub(crate) async fn send(
    client: &reqwest::Client,
    route: &Route,
    request: &Request,
) -> Result<ModelStream, ProviderError> {
    let deadline = Instant::now() + route.timeouts.request;
    let body = serde_json::to_vec(&body(route, request)).expect("JSON values serialize");
    let mut builder = client
        .post(wire::endpoint(route.wire_format, &route.base_url))
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .header("accept", "text/event-stream")
        .body(body);
    builder = authenticate(builder, route);
    let response = match timeout_at(deadline, builder.send()).await {
        Err(_) => return Err(timed_out("no response within the request timeout")),
        Ok(Err(err)) => return Err(transport_error(&err)),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let text = timeout_at(deadline, response.text())
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
    let mut stream = ModelStream {
        response: Some(response),
        parser: SseParser::default(),
        decoder: wire::decoder(route.wire_format),
        normalizer: Normalizer::default(),
        pending: VecDeque::new(),
        deadline,
        idle: route.timeouts.idle,
        failure: None,
        usage: None,
        ledger: None,
        recorded: None,
    };
    stream.fill().await?;
    Ok(stream)
}

fn authenticate(builder: reqwest::RequestBuilder, route: &Route) -> reqwest::RequestBuilder {
    let builder = if route.wire_format == WireFormat::AnthropicMessages {
        builder.header("anthropic-version", wire::ANTHROPIC_VERSION)
    } else {
        builder
    };
    let Some(secret) = route.credential.value() else {
        return builder;
    };
    let mut value =
        if route.wire_format == WireFormat::AnthropicMessages && !route.credential.is_bearer() {
            HeaderValue::from_str(secret.expose())
        } else {
            HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        }
        .unwrap_or_else(|_| HeaderValue::from_static(""));
    value.set_sensitive(true);
    if route.wire_format == WireFormat::AnthropicMessages && !route.credential.is_bearer() {
        return builder.header("x-api-key", value);
    }
    let builder = builder.header(reqwest::header::AUTHORIZATION, value);
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

fn timed_out(message: &str) -> ProviderError {
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

/// A started response: normalised events in order, ending with exactly
/// one `stop` (`stop_reason = "error"` when the stream failed after it
/// started; [`failure`](Self::failure) then says why).
#[derive(Debug)]
pub struct ModelStream {
    response: Option<reqwest::Response>,
    parser: SseParser,
    decoder: Box<dyn Decode>,
    normalizer: Normalizer,
    pending: VecDeque<StreamEvent>,
    deadline: Instant,
    idle: Duration,
    failure: Option<ProviderError>,
    usage: Option<Usage>,
    ledger: Option<Pending>,
    recorded: Option<Result<pmt_store::UsageEvent, pmt_core::Error>>,
}

impl std::fmt::Debug for dyn Decode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Decode")
    }
}

impl ModelStream {
    /// The next event; `None` after `stop`.
    pub async fn next(&mut self) -> Option<StreamEvent> {
        if self.pending.is_empty()
            && !self.normalizer.is_finished()
            && let Err(error) = self.fill().await
        {
            self.fail(error);
        }
        let event = self.pending.pop_front()?;
        if let StreamEvent::Usage { usage } = &event {
            self.usage = Some(*usage);
            if let Some(pending) = self.ledger.take() {
                let recorded = pending.record(usage);
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
            let chunk = self.read_chunk().await?;
            self.consume(chunk.as_deref())?;
            self.pending.extend(self.normalizer.drain());
        }
        Ok(())
    }

    /// The next body chunk (`None` at the end) within the idle and request
    /// deadlines.
    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, ProviderError> {
        let Some(response) = self.response.as_mut() else {
            return Err(wire::truncated());
        };
        let idle_deadline = (Instant::now() + self.idle).min(self.deadline);
        match timeout_at(idle_deadline, response.chunk()).await {
            Err(_) if idle_deadline < self.deadline => {
                Err(timed_out("no stream event within the idle timeout"))
            }
            Err(_) => Err(timed_out(
                "the response did not finish within the request timeout",
            )),
            Ok(Err(err)) => Err(transport_error(&err)),
            Ok(Ok(chunk)) => Ok(chunk.map(|bytes| bytes.to_vec())),
        }
    }

    /// Decodes `chunk`, or finishes the stream at its end.
    fn consume(&mut self, chunk: Option<&[u8]>) -> Result<(), ProviderError> {
        let Some(bytes) = chunk else {
            self.response = None;
            if let Some(event) = self.parser.finish() {
                self.decoder.event(&event, &mut self.normalizer)?;
            }
            if !self.normalizer.is_finished() {
                self.decoder.end(&mut self.normalizer)?;
            }
            return Ok(());
        };
        for event in self.parser.feed(bytes) {
            self.decoder.event(&event, &mut self.normalizer)?;
        }
        Ok(())
    }

    fn fail(&mut self, error: ProviderError) {
        self.response = None;
        self.failure = Some(error);
        self.normalizer.finish(Some(StopReason::Error));
        self.pending.extend(self.normalizer.drain());
    }
}
