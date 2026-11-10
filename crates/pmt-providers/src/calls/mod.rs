//! What happens around every model call besides sending it (spec:
//! replay-and-evaluation, Content-addressed model-call cache; Session
//! recordings; Replay modes): a replay source may answer it, the
//! profile's [`cache`] may answer it or store its response, and a
//! [`Recorder`] keeps it in the session's recording.
//!
//! Both the main [`crate::ModelCaller`] and the auxiliary caller send
//! through the same point, so every call of a session, main or auxiliary,
//! is keyed, served, cached and recorded the same way. A served response
//! is replayed with its recorded chunk boundaries and is billed as a cache
//! hit (`cache_hit = true`, `cost_usd = 0`); nothing is served or stored
//! for a failed response.

pub mod cache;

use std::sync::Arc;

use pmt_core::id::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical::{self, CallTarget};
use crate::classify::{ErrorClass, ProviderError};
use crate::event::{StreamEvent, Usage};
use crate::request::Request;
use crate::resolve::{BoxFuture, Route};

pub use cache::ModelCallCache;

/// One response as it streamed: its normalised events in the chunks they
/// arrived in, its usage, and how it failed when it did (a recording keeps
/// failures so a replay takes the same retry, fallback and compression
/// paths; the cache never stores them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    /// The events, chunk by chunk (a successful response's last chunk ends
    /// with `stop`).
    pub chunks: Vec<Vec<StreamEvent>>,
    /// The usage the response reported.
    pub usage: Usage,
    /// The failure: before anything streamed (no chunks), or after them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

/// A recorded provider failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The class ([`ErrorClass::as_str`]).
    pub class: String,
    /// The HTTP status.
    pub status: Option<u16>,
    /// What failed.
    pub message: String,
    /// The provider's `Retry-After`, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl Failure {
    /// The record of `error`.
    #[must_use]
    pub fn of(error: &ProviderError) -> Self {
        Self {
            class: error.class.as_str().to_owned(),
            status: error.status,
            message: error.message.clone(),
            retry_after_ms: error
                .retry_after
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    /// The failure as the caller sees it again.
    #[must_use]
    pub fn to_error(&self) -> ProviderError {
        let class = ErrorClass::parse(&self.class).unwrap_or(ErrorClass::ServerError);
        ProviderError {
            status: self.status,
            retry_after: self.retry_after_ms.map(std::time::Duration::from_millis),
            ..ProviderError::new(class, self.message.clone())
        }
    }
}

impl Recorded {
    /// Every event in order.
    pub fn events(&self) -> impl Iterator<Item = &StreamEvent> {
        self.chunks.iter().flatten()
    }
}

/// A model call's identity: its canonical request and hash.
#[derive(Debug, Clone, PartialEq)]
pub struct CallKey {
    /// `sha256:<hex>` of [`Self::canonical`].
    pub hash: String,
    /// The canonical request (see [`crate::canonical`]).
    pub canonical: Value,
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
}

impl CallKey {
    /// The key of `request` sent on `route`.
    #[must_use]
    pub fn of(route: &Route, request: &Request) -> Self {
        let canonical = canonical::canonical(request, &CallTarget::of(route));
        Self {
            hash: canonical::hash_value(&canonical),
            canonical,
            provider: route.provider.clone(),
            model: route.model.clone(),
        }
    }
}

/// What a replay source says about a call.
#[derive(Debug)]
pub enum Answer {
    /// Serve this recorded response; nothing is sent.
    Serve(Recorded),
    /// Send the call for real (no recording applies, or a divergence fell
    /// through to the live provider).
    Live,
    /// Refuse the call (a divergence that stops the replay).
    Refuse(ProviderError),
}

/// Answers model calls from a recording (spec: replay-and-evaluation,
/// Replay modes; Divergence detection).
pub trait ReplaySource: Send + Sync {
    /// The answer to the call keyed `key`.
    fn answer<'a>(&'a self, key: &'a CallKey) -> BoxFuture<'a, Answer>;
}

/// Keeps each successful call of a session in its recording (spec:
/// replay-and-evaluation, Session recordings).
pub trait Recorder: Send + Sync {
    /// `session`'s call keyed `key` answered `response` (`served`: from a
    /// replay or the cache).
    fn record(&self, session: SessionId, key: &CallKey, response: &Recorded, served: bool);
}

/// The layers around a profile's model calls.
#[derive(Clone, Default)]
pub struct Calls {
    /// Answers calls first (a replay).
    pub replay: Option<Arc<dyn ReplaySource>>,
    /// The profile's model-call cache, when on.
    pub cache: Option<Arc<ModelCallCache>>,
    /// The session recordings.
    pub recorder: Option<Arc<dyn Recorder>>,
}

impl std::fmt::Debug for Calls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calls")
            .field("replay", &self.replay.is_some())
            .field("cache", &self.cache)
            .field("recorder", &self.recorder.is_some())
            .finish()
    }
}

/// How [`Calls::before`] decided.
pub(crate) enum Before {
    /// Serve this response.
    Serve(Recorded),
    /// Send the call.
    Send,
    /// Refuse it.
    Refuse(ProviderError),
}

impl Calls {
    /// Whether any layer is set (otherwise calls are not keyed at all).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replay.is_none() && self.cache.is_none() && self.recorder.is_none()
    }

    /// The replay's answer, else a cache hit, else send.
    pub(crate) async fn before(&self, key: &CallKey) -> Before {
        if let Some(replay) = &self.replay {
            match replay.answer(key).await {
                Answer::Serve(recorded) => return Before::Serve(recorded),
                Answer::Refuse(error) => return Before::Refuse(error),
                Answer::Live => {}
            }
        }
        match self.cache.as_ref().and_then(|cache| cache.get(&key.hash)) {
            Some(recorded) => Before::Serve(recorded),
            None => Before::Send,
        }
    }

    /// Records a response, and caches it when it was sent and succeeded.
    pub(crate) fn after(
        &self,
        session: Option<SessionId>,
        key: &CallKey,
        response: &Recorded,
        served: bool,
    ) {
        let succeeded = response.failure.is_none();
        if succeeded
            && !served
            && let Some(cache) = &self.cache
        {
            let target = (key.provider.as_str(), key.model.as_str());
            if let Err(err) = cache.put(&key.hash, target, response) {
                tracing::warn!(error = %err, "model-call cache write failed");
            }
        }
        if let (Some(recorder), Some(session)) = (&self.recorder, session) {
            recorder.record(session, key, response, served);
        }
    }

    /// Records a call that failed before anything streamed.
    pub(crate) fn failed(&self, session: Option<SessionId>, key: &CallKey, error: &ProviderError) {
        let response = Recorded {
            chunks: Vec::new(),
            usage: Usage::default(),
            failure: Some(Failure::of(error)),
        };
        self.after(session, key, &response, false);
    }
}

/// The profile's session recordings keep every call.
impl Recorder for pmt_obs::recording::Writer {
    fn record(&self, session: SessionId, key: &CallKey, response: &Recorded, served: bool) {
        self.call(
            &session.to_string(),
            &pmt_obs::recording::Call {
                ts: jiff::Timestamp::now().to_string(),
                request_hash: key.hash.clone(),
                request: key.canonical.clone(),
                response: serde_json::to_value(response).unwrap_or(Value::Null),
                served,
            },
        );
    }
}
