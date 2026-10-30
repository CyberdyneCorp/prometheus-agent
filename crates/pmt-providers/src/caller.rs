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
/// Returns the stream and the route it went out on.
///
/// # Errors
///
/// The classified failure, or the hooks' refusal.
pub(crate) async fn send_once(
    sender: &Sender,
    hooks: Option<&dyn CredentialHooks>,
    route: &Route,
    request: &Request,
) -> Result<(ModelStream, Route), ProviderError> {
    let Some(hooks) = hooks else {
        return Ok((sender.send(route, request).await?, route.clone()));
    };
    let mut may_recover = true;
    loop {
        let prepared = hooks.prepare(route).await?;
        let error = match sender.send(&prepared, request).await {
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
        }
    }

    /// Runs `hooks` around every request (pools, benches, refresh).
    #[must_use]
    pub fn with_credentials(mut self, hooks: Arc<dyn CredentialHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Serves routes of the built-in `mock` provider from `mock` (the
    /// session's script; without one they echo).
    #[must_use]
    pub fn with_mock(mut self, mock: Arc<Mock>) -> Self {
        self.sender = self.sender.with_mock(mock);
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
        let mut retries = route.retry;
        if let Some(cap) = self.attempt_cap {
            retries.max_retries = retries.max_retries.min(cap - 1);
        }
        let mut retry = 0;
        loop {
            let error = match send_once(&self.sender, self.hooks.as_deref(), route, request).await {
                Ok((mut stream, prepared)) => {
                    if let (Some(recorder), Some(ctx)) = (&self.recorder, ctx) {
                        stream.record_with(Pending::new(recorder.clone(), &prepared, request, ctx));
                    }
                    return Ok(stream);
                }
                Err(error) if error.refusal.is_some() => return Err(error),
                Err(error) => error,
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
