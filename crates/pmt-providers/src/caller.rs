//! Making a model call (spec: model-providers, Error classification and
//! same-route retries; In-turn fallback activation; agent-turn-loop, Model
//! call retries): retries on the same route with jittered backoff, then
//! the fallback chain, recording one ledger row per call that reports
//! usage.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_obs::usage::UsageRecorder;

use crate::capability;
use crate::classify::ProviderError;
use crate::cost::{CallContext, Pending};
use crate::fallback::{Activated, Fallback, Walk};
use crate::request::Request;
use crate::resolve::Route;
use crate::transport::{self, ModelStream};

/// Sends model requests for one session.
#[derive(Debug, Clone)]
pub struct ModelCaller {
    client: reqwest::Client,
    attempt_cap: Option<u32>,
    recorder: Option<Arc<UsageRecorder>>,
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
            client,
            attempt_cap: None,
            recorder: None,
        }
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
    /// happen before anything was streamed.
    ///
    /// # Errors
    ///
    /// The last attempt's classified failure.
    pub async fn send(
        &self,
        route: &Route,
        request: &Request,
        ctx: Option<&CallContext>,
    ) -> Result<ModelStream, ProviderError> {
        let mut retries = route.retry;
        if let Some(cap) = self.attempt_cap {
            retries.max_retries = retries.max_retries.min(cap - 1);
        }
        let mut retry = 0;
        loop {
            match transport::send(&self.client, route, request).await {
                Ok(mut stream) => {
                    if let (Some(recorder), Some(ctx)) = (&self.recorder, ctx) {
                        stream.record_with(Pending::new(recorder.clone(), route, request, ctx));
                    }
                    return Ok(stream);
                }
                Err(error) => {
                    let Some(delay) = retries.delay(retry, &error) else {
                        return Err(error);
                    };
                    tracing::debug!(provider = %route.provider, class = error.class.as_str(), ?delay, "retrying model call");
                    tokio::time::sleep(delay).await;
                    retry += 1;
                }
            }
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
        let (meta, _) =
            walk.catalog
                .metadata_at(&current.provider, &current.model, Some(&current.base_url));
        capability::check(&current.label(), &meta, &walk.needs).map_err(CallError::Refused)?;
        let mut fallbacks = Vec::new();
        let mut first: Option<(ProviderError, Route)> = None;
        loop {
            let route = state.current().clone();
            match self.send(&route, request, ctx).await {
                Ok(stream) => {
                    state.primary_succeeded();
                    return Ok(Called {
                        stream,
                        route,
                        fallbacks,
                    });
                }
                Err(error) => {
                    let activated = state.activate(&error, walk, Timestamp::now()).await;
                    let (error, route) = first.get_or_insert((error, route)).clone();
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
