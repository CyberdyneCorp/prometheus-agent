//! Per-route request limits (spec: platform-adapters, Webhook limits,
//! idempotency and responses): a fixed one-minute window per route, and
//! delivery ids remembered for an hour so a redelivery starts nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// How long a delivery id is remembered.
pub(crate) const IDEMPOTENCY_WINDOW: Duration = Duration::from_hours(1);

/// Delivery ids remembered at most (the oldest go first).
const MAX_DELIVERIES: usize = 10_000;

/// The window counters and remembered deliveries of one adapter.
#[derive(Debug, Default)]
pub(crate) struct Guard {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
    seen: Mutex<VecDeque<(Instant, String)>>,
}

impl Guard {
    /// Counts a request on `route`; `false` once the minute's `limit` is
    /// used.
    pub(crate) fn admit(&self, route: &str, limit: u32, now: Instant) -> bool {
        let mut windows = self.windows.lock().expect("windows poisoned");
        let window = windows.entry(route.to_owned()).or_insert((now, 0));
        if now.duration_since(window.0) >= Duration::from_mins(1) {
            *window = (now, 0);
        }
        if window.1 >= limit {
            return false;
        }
        window.1 += 1;
        true
    }

    /// Whether `route`'s delivery `id` was handled within the window.
    pub(crate) fn seen(&self, route: &str, id: &str, now: Instant) -> bool {
        let key = format!("{route}\u{1f}{id}");
        let mut seen = self.seen.lock().expect("seen poisoned");
        while seen
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= IDEMPOTENCY_WINDOW)
        {
            seen.pop_front();
        }
        seen.iter().any(|(_, k)| *k == key)
    }

    /// Remembers that `route`'s delivery `id` was handled.
    pub(crate) fn remember(&self, route: &str, id: &str, now: Instant) {
        let mut seen = self.seen.lock().expect("seen poisoned");
        if seen.len() >= MAX_DELIVERIES {
            seen.pop_front();
        }
        seen.push_back((now, format!("{route}\u{1f}{id}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minute_window_resets() {
        let guard = Guard::default();
        let start = Instant::now();
        assert!(guard.admit("ci", 2, start));
        assert!(guard.admit("ci", 2, start));
        assert!(!guard.admit("ci", 2, start + Duration::from_secs(59)));
        assert!(guard.admit("other", 2, start));
        assert!(guard.admit("ci", 2, start + Duration::from_mins(1)));
    }

    #[test]
    fn deliveries_are_remembered_for_an_hour() {
        let guard = Guard::default();
        let start = Instant::now();
        guard.remember("ci", "d1", start);
        assert!(guard.seen("ci", "d1", start + Duration::from_secs(3599)));
        assert!(!guard.seen("pr", "d1", start));
        assert!(!guard.seen("ci", "d1", start + IDEMPOTENCY_WINDOW));
    }
}
