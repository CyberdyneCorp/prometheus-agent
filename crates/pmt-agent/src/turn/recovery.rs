//! Model calls with auto-recovery (spec: agent-turn-loop, Model call
//! retries and auto-recovery).
//!
//! Same-route retries (`agent.api_max_retries` attempts, jittered
//! backoff) and the fallback chain run inside the provider layer. When
//! both are spent on a transient failure and no answer text reached the
//! user, the turn waits and calls again, up to `agent.auto_recovery_cycles`
//! times at 15/30/60/60/60 s (a provider `Retry-After` up to 120 s wins).
//! Every other failure ends the turn at once: non-retryable classes (auth,
//! request format, content policy, billing, entitlement) with
//! `provider_failed`, an overflow with `context_exhausted`.

use std::time::Duration;

use pmt_obs::trajectory::events::Event;
use pmt_providers::classify::ErrorClass;
use pmt_providers::cost::{CallContext, StaleCap};
use pmt_providers::request::Request;
use tokio::time::Instant;

use super::exit::ExitReason;
use super::model::{ModelFailure, ModelReply};
use super::run::{Ending, Turn};

/// Waits between recovery cycles; later cycles reuse the last one.
const SCHEDULE: [Duration; 5] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_mins(1),
    Duration::from_mins(1),
    Duration::from_mins(1),
];
/// The longest `Retry-After` honoured between cycles.
const RETRY_AFTER_MAX: Duration = Duration::from_mins(2);
/// The floor of a capped stale-call timeout.
const STALE_FLOOR: Duration = Duration::from_mins(1);

/// The wait before recovery cycle `cycle` (0-based) after `failure`, or
/// `None` when the turn must end.
pub(super) fn recovery_wait(failure: &ModelFailure, cycle: u32, cycles: u32) -> Option<Duration> {
    let transient = failure.class.is_some_and(ErrorClass::is_retryable);
    if !transient || failure.streamed || cycle >= cycles {
        return None;
    }
    let scheduled =
        SCHEDULE[usize::try_from(cycle).map_or(SCHEDULE.len() - 1, |c| c.min(SCHEDULE.len() - 1))];
    Some(
        failure
            .retry_after
            .map_or(scheduled, |after| after.min(RETRY_AFTER_MAX)),
    )
}

/// The cap on implicit stale-call timeouts with `remaining` run budget:
/// `max(60 s, remaining / 2)`.
pub(super) fn stale_cap(remaining: Duration) -> Duration {
    (remaining / 2).max(STALE_FLOOR)
}

impl Turn<'_> {
    /// Sends `request`, racing any stop and the run budget, and recovers
    /// from transient failures.
    pub(super) async fn call(&mut self, request: &Request) -> Result<ModelReply, Ending> {
        let mut cycle = 0;
        loop {
            let failure = match self.call_once(request).await? {
                Ok(reply) => return Ok(reply),
                Err(failure) => failure,
            };
            for fallback in &failure.fallbacks {
                self.emit(Event::ProviderFallback(fallback.clone())).await;
            }
            let cycles = self.config.agent.auto_recovery_cycles;
            let Some(wait) = recovery_wait(&failure, cycle, cycles) else {
                return Err(ended(failure));
            };
            tracing::warn!(
                error = %failure.error,
                cycle = cycle + 1,
                ?wait,
                "model call failed after retries and fallbacks; waiting to recover"
            );
            self.race_model(tokio::time::sleep(wait)).await?;
            cycle += 1;
        }
    }

    async fn call_once(
        &self,
        request: &Request,
    ) -> Result<Result<ModelReply, ModelFailure>, Ending> {
        let setup = &self.agent.setup;
        let mut ctx = CallContext::new(setup.session, "main");
        ctx.turn_id = Some(self.id.to_string());
        ctx.stale_cap = self.stale_cap();
        let deltas = self.options.deltas.clone();
        let model = setup.model.clone();
        self.race_model(model.call(request, &ctx, &*deltas)).await
    }

    /// The stale-call cap under a run budget (no clock read without one).
    fn stale_cap(&self) -> Option<StaleCap> {
        let clock = self.clock?;
        let remaining = clock.deadline().saturating_duration_since(Instant::now());
        let config = self.agent.setup.scope.layers();
        let explicit = self
            .config
            .providers
            .keys()
            .filter(|id| {
                config.source(&format!("providers.{id}.stream_idle_timeout_secs"))
                    != pmt_config::Source::Default
            })
            .cloned()
            .collect();
        Some(StaleCap {
            cap: stale_cap(remaining),
            explicit,
        })
    }
}

/// The ending of an unrecovered failure.
fn ended(failure: ModelFailure) -> Ending {
    let reason = if failure.class == Some(ErrorClass::ContextOverflow) {
        ExitReason::ContextExhausted
    } else {
        ExitReason::ProviderFailed
    };
    Ending::failed(reason, failure.error)
}

#[cfg(test)]
mod tests {
    use pmt_core::error::{Error, codes};

    use super::*;

    fn failure(class: ErrorClass) -> ModelFailure {
        ModelFailure::new(Error::new(&codes::PROVIDER_CALL_FAILED, "x"), Some(class))
    }

    #[test]
    fn transient_failures_wait_on_the_schedule() {
        let f = failure(ErrorClass::ServerError);
        let waits: Vec<u64> = (0..6)
            .map(|c| recovery_wait(&f, c, 6).unwrap().as_secs())
            .collect();
        assert_eq!(waits, [15, 30, 60, 60, 60, 60]);
        assert_eq!(recovery_wait(&f, 5, 5), None, "cycles spent");
        assert_eq!(recovery_wait(&f, 0, 0), None, "0 disables");
    }

    #[test]
    fn retry_after_is_honoured_up_to_two_minutes() {
        let mut f = failure(ErrorClass::RateLimit);
        f.retry_after = Some(Duration::from_secs(7));
        assert_eq!(recovery_wait(&f, 0, 5), Some(Duration::from_secs(7)));
        f.retry_after = Some(Duration::from_mins(10));
        assert_eq!(recovery_wait(&f, 0, 5), Some(Duration::from_mins(2)));
    }

    #[test]
    fn permanent_or_streamed_failures_end_the_turn() {
        for class in [
            ErrorClass::Auth,
            ErrorClass::InvalidRequest,
            ErrorClass::ContentFilter,
            ErrorClass::Billing,
            ErrorClass::Entitlement,
            ErrorClass::ContextOverflow,
        ] {
            assert_eq!(recovery_wait(&failure(class), 0, 5), None, "{class:?}");
        }
        let mut streamed = failure(ErrorClass::Network);
        streamed.streamed = true;
        assert_eq!(recovery_wait(&streamed, 0, 5), None);
    }

    #[test]
    fn the_stale_cap_is_half_the_remaining_budget_but_at_least_a_minute() {
        assert_eq!(stale_cap(Duration::from_mins(5)), Duration::from_secs(150));
        assert_eq!(stale_cap(Duration::from_secs(50)), Duration::from_mins(1));
    }
}
