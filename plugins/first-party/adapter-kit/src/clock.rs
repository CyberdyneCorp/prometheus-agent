//! Time as the plugin sees it: the host polls every 250 ms, so the polls
//! counted so far are the plugin's clock (the sandbox has no other).

use std::sync::atomic::{AtomicU64, Ordering};

/// How often the host polls.
pub const POLL_MS: u64 = 250;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Counts one poll; call it first thing in `poll`.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Milliseconds since the plugin instance started (in polls).
#[must_use]
pub fn now_ms() -> u64 {
    TICKS.load(Ordering::Relaxed) * POLL_MS
}

/// Whether `secs` seconds have passed since `since_ms`.
#[must_use]
pub fn elapsed(since_ms: u64, secs: u64) -> bool {
    now_ms().saturating_sub(since_ms) >= secs * 1000
}
