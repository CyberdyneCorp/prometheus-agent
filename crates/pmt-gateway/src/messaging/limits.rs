//! Per-sender rate limits (spec: messaging-gateway, Per-user rate
//! limiting): token buckets keyed by profile, platform and sender, one for
//! messages (`burst` deep, refilled at `messages_per_minute`) and one for
//! turn-starting messages (`turns_per_hour`). An event over a limit is
//! dropped, not queued, and the sender hears about it at most once per
//! window: until the bucket has a token again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use pmt_config::schema::RateLimitConfig;
use tokio::time::Instant;

/// What the sender is told when a limit drops their message.
#[must_use]
pub(crate) fn slow_down(wait: Duration) -> String {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    format!(
        "Slow down: rate limit reached, try again in {}s.",
        secs.max(1)
    )
}

/// What the sender is told when their daily spend reached the cap.
#[must_use]
pub(crate) fn daily_budget(spent: f64, cap: f64) -> String {
    format!("Daily budget reached ({spent:.2} of {cap:.2} USD); it resets at local midnight.")
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn full(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last: now,
        }
    }

    /// Refills to `now`; the wait until one token is available (zero when
    /// one is).
    fn wait(&mut self, capacity: f64, per_sec: f64, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_sec).min(capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / per_sec)
        }
    }
}

#[derive(Debug)]
struct Sender {
    messages: Bucket,
    turns: Bucket,
    /// No further slow-down reply before this.
    quiet_until: Option<Instant>,
}

/// The outcome of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Within the limits (the tokens are taken).
    Pass,
    /// Over a limit: drop it; `notify` the sender this time.
    Limited {
        /// Until the next token.
        wait: Duration,
        /// The first drop of this window.
        notify: bool,
    },
}

/// Every sender's buckets.
#[derive(Debug, Default)]
pub(crate) struct Limits {
    senders: Mutex<HashMap<String, Sender>>,
    /// When each sender was last told their daily budget is spent.
    budget_told: Mutex<HashMap<String, Instant>>,
}

/// Least time between two daily-budget notices to one sender.
const BUDGET_NOTICE_EVERY: Duration = Duration::from_hours(1);

impl Limits {
    /// Whether `sender` should hear about the spent daily budget now
    /// (once an hour).
    pub(crate) fn budget_notice_due(&self, sender: &str, now: Instant) -> bool {
        let mut told = self.budget_told.lock().expect("limits poisoned");
        let due = told
            .get(sender)
            .is_none_or(|at| now.saturating_duration_since(*at) >= BUDGET_NOTICE_EVERY);
        if due {
            told.insert(sender.to_owned(), now);
        }
        due
    }

    /// Checks (and on success takes) the tokens of one event of `sender`
    /// (`turn`: it would start or join a turn).
    pub(crate) fn check(
        &self,
        sender: &str,
        config: &RateLimitConfig,
        turn: bool,
        now: Instant,
    ) -> Verdict {
        let burst = f64::from(config.burst.max(1));
        let per_minute = f64::from(config.messages_per_minute.max(1)) / 60.0;
        let hourly = f64::from(config.turns_per_hour.max(1));
        let per_hour = hourly / 3600.0;
        let mut senders = self.senders.lock().expect("limits poisoned");
        let state = senders.entry(sender.to_owned()).or_insert_with(|| Sender {
            messages: Bucket::full(burst, now),
            turns: Bucket::full(hourly, now),
            quiet_until: None,
        });
        let mut wait = state.messages.wait(burst, per_minute, now);
        if turn {
            wait = wait.max(state.turns.wait(hourly, per_hour, now));
        }
        if wait.is_zero() {
            state.messages.tokens -= 1.0;
            if turn {
                state.turns.tokens -= 1.0;
            }
            return Verdict::Pass;
        }
        let notify = state.quiet_until.is_none_or(|until| now >= until);
        if notify {
            state.quiet_until = Some(now + wait);
        }
        Verdict::Limited { wait, notify }
    }

    /// Forgets senders idle for an hour (their buckets are full again).
    pub(crate) fn sweep(&self, now: Instant) {
        self.senders
            .lock()
            .expect("limits poisoned")
            .retain(|_, s| {
                now.saturating_duration_since(s.messages.last) < Duration::from_hours(1)
            });
        self.budget_told
            .lock()
            .expect("limits poisoned")
            .retain(|_, at| now.saturating_duration_since(*at) < BUDGET_NOTICE_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_admitted_then_refilled_at_the_rate() {
        let limits = Limits::default();
        let config = RateLimitConfig::default();
        let start = Instant::now();
        let mut admitted = 0;
        let mut notices = 0;
        // 40 messages in 10 seconds.
        for i in 0..40u64 {
            let at = start + Duration::from_millis(i * 250);
            match limits.check("u", &config, true, at) {
                Verdict::Pass => admitted += 1,
                Verdict::Limited { notify, .. } => notices += u32::from(notify),
            }
        }
        // The burst (10) plus 20/minute over 10 seconds (3).
        assert_eq!(admitted, 13);
        assert!(notices <= 4, "{notices}");
        assert!(notices >= 1);
    }

    #[test]
    fn one_notice_per_window() {
        let limits = Limits::default();
        let config = RateLimitConfig {
            burst: 1,
            messages_per_minute: 1,
            ..RateLimitConfig::default()
        };
        let now = Instant::now();
        assert_eq!(limits.check("u", &config, false, now), Verdict::Pass);
        let Verdict::Limited { wait, notify } = limits.check("u", &config, false, now) else {
            panic!("over the limit");
        };
        assert!(notify);
        assert_eq!(wait, Duration::from_mins(1));
        for secs in [1, 30, 59] {
            assert!(matches!(
                limits.check("u", &config, false, now + Duration::from_secs(secs)),
                Verdict::Limited { notify: false, .. }
            ));
        }
        assert_eq!(
            limits.check("u", &config, false, now + Duration::from_secs(61)),
            Verdict::Pass
        );
        assert_eq!(limits.check("other", &config, false, now), Verdict::Pass);
    }

    #[test]
    fn turns_have_their_own_hourly_bucket() {
        let limits = Limits::default();
        let config = RateLimitConfig {
            burst: 100,
            messages_per_minute: 1000,
            turns_per_hour: 2,
            daily_cost_usd: None,
        };
        let now = Instant::now();
        assert_eq!(limits.check("u", &config, true, now), Verdict::Pass);
        assert_eq!(limits.check("u", &config, true, now), Verdict::Pass);
        assert!(matches!(
            limits.check("u", &config, true, now),
            Verdict::Limited { .. }
        ));
        assert_eq!(
            limits.check("u", &config, false, now),
            Verdict::Pass,
            "a command starts no turn"
        );
    }

    #[test]
    fn the_notice_rounds_up() {
        assert_eq!(
            slow_down(Duration::from_millis(2100)),
            "Slow down: rate limit reached, try again in 3s."
        );
    }
}
