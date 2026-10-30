//! Making an auxiliary call: each attempt runs under the task's timeout
//! and is bracketed by `aux_call.start` / `aux_call.end`; a retryable
//! failure is retried on the same route `auxiliary.transient_retries`
//! times; each attempt that reports usage writes one ledger row with
//! purpose `auxiliary.<task>` under the owning session. Auxiliary calls
//! never emit `model.call`.

use std::sync::Arc;
use std::time::Instant;

use pmt_core::error::Error;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{AuxCallEnd, AuxCallStart, Failure, Tokens};
use pmt_obs::usage::UsageRecorder;

use super::AuxRoute;
use crate::caller::{CredentialHooks, send_once};
use crate::classify::{ErrorClass, ProviderError, RetryPolicy};
use crate::cost::{CallContext, Pending};
use crate::event::{StreamEvent, Usage};
use crate::request::Request;

/// Receives the `aux_call.*` events of a session (its trajectory writer).
pub trait AuxEvents: Send + Sync {
    /// One event.
    fn emit(&self, event: Event);
}

/// A completed auxiliary call.
#[derive(Debug, Clone, PartialEq)]
pub struct AuxResponse {
    /// The concatenated text.
    pub text: String,
    /// Every normalised event of the successful attempt.
    pub events: Vec<StreamEvent>,
    /// The reported usage.
    pub usage: Option<Usage>,
    /// Attempts made (1 when the first one succeeded).
    pub attempts: u32,
}

/// Makes auxiliary calls for one session.
#[derive(Clone)]
pub struct AuxCaller {
    client: reqwest::Client,
    recorder: Option<Arc<UsageRecorder>>,
    events: Option<Arc<dyn AuxEvents>>,
    hooks: Option<Arc<dyn CredentialHooks>>,
}

impl std::fmt::Debug for AuxCaller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxCaller").finish_non_exhaustive()
    }
}

/// One attempt's outcome.
struct Attempt {
    result: Result<(Vec<StreamEvent>, Option<Usage>), ProviderError>,
    cost_usd: Option<f64>,
}

impl AuxCaller {
    /// A caller over `client` (from `pmt_net::client_builder`).
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            recorder: None,
            events: None,
            hooks: None,
        }
    }

    /// Runs `hooks` around every request (pools, benches, refresh), as
    /// the main caller does.
    #[must_use]
    pub fn with_credentials(mut self, hooks: Arc<dyn CredentialHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Records every attempt's usage in the ledger.
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<UsageRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Sends the `aux_call.*` events to `events`.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn AuxEvents>) -> Self {
        self.events = Some(events);
        self
    }

    /// Sends `request` on `aux` (the route's model replaces
    /// `request.model`) for the session of `ctx` (its purpose is replaced
    /// by `auxiliary.<task>`).
    ///
    /// # Errors
    ///
    /// The last attempt's classified failure (`provider.*`).
    pub async fn call(
        &self,
        aux: &AuxRoute,
        request: &Request,
        ctx: &CallContext,
    ) -> Result<AuxResponse, Error> {
        let mut ctx = ctx.clone();
        ctx.purpose = aux.task.purpose();
        let policy = RetryPolicy {
            max_retries: aux.retries,
            ..aux.route.retry
        };
        let mut retry = 0;
        loop {
            let call_id = format!("call_{}", ulid::Ulid::generate().to_string().to_lowercase());
            self.emit(Event::AuxCallStart(AuxCallStart {
                call_id: call_id.clone(),
                purpose: ctx.purpose.clone(),
                provider: aux.route.provider.clone(),
                model: aux.route.model.clone(),
                attempt: retry + 1,
            }));
            let started = Instant::now();
            let attempt = self.attempt(aux, request, &ctx, &call_id).await;
            self.emit_end(&call_id, started, &attempt);
            match attempt.result {
                Ok((events, usage)) => {
                    return Ok(AuxResponse {
                        text: text_of(&events),
                        events,
                        usage,
                        attempts: retry + 1,
                    });
                }
                Err(error) => {
                    let Some(delay) = policy.delay(retry, &error) else {
                        return Err(error.to_route_error(&aux.route));
                    };
                    tokio::time::sleep(delay).await;
                    retry += 1;
                }
            }
        }
    }

    async fn attempt(
        &self,
        aux: &AuxRoute,
        request: &Request,
        ctx: &CallContext,
        call_id: &str,
    ) -> Attempt {
        let run = async {
            let (mut stream, route) =
                send_once(&self.client, self.hooks.as_deref(), &aux.route, request)
                    .await
                    .map_err(|e| (e, None))?;
            if let Some(recorder) = &self.recorder {
                let pending = Pending::new(recorder.clone(), &route, request, ctx);
                stream.record_with(pending.with_call_id(call_id));
            }
            let events = stream.collect().await;
            let cost_usd = stream
                .recorded()
                .and_then(|r| r.as_ref().ok())
                .and_then(|row| row.cost_usd);
            match stream.failure() {
                Some(failure) => Err((failure.clone(), cost_usd)),
                None => Ok((events, stream.usage(), cost_usd)),
            }
        };
        match tokio::time::timeout(aux.timeout, run).await {
            Ok(Ok((events, usage, cost_usd))) => Attempt {
                result: Ok((events, usage)),
                cost_usd,
            },
            Ok(Err((error, cost_usd))) => Attempt {
                result: Err(error),
                cost_usd,
            },
            Err(_) => Attempt {
                result: Err(ProviderError::new(
                    ErrorClass::Timeout,
                    format!(
                        "auxiliary task '{}' did not finish within {} s",
                        aux.task.as_str(),
                        aux.timeout.as_secs()
                    ),
                )),
                cost_usd: None,
            },
        }
    }

    fn emit_end(&self, call_id: &str, started: Instant, attempt: &Attempt) {
        let usage = attempt.result.as_ref().ok().and_then(|(_, usage)| *usage);
        let error = attempt.result.as_ref().err().map(|e| Failure {
            kind: e.class.as_str().to_owned(),
            code: None,
            message: e.message.clone(),
        });
        self.emit(Event::AuxCallEnd(AuxCallEnd {
            call_id: call_id.to_owned(),
            ok: attempt.result.is_ok(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            tokens: usage.map(|u| Tokens::from(u.tokens())).unwrap_or_default(),
            cost_usd: attempt.cost_usd,
            error,
        }));
    }

    fn emit(&self, event: Event) {
        if let Some(events) = &self.events {
            events.emit(event);
        }
    }
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
