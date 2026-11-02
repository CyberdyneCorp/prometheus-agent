//! Inbound deduplication (spec: platform-adapters, Inbound deduplication
//! and text batching in the host; Typed admission result): an event whose
//! `(profile, platform, chat id, platform message id)` was admitted within
//! `gateway.dedup.ttl_secs` is a `duplicate` and starts nothing; each
//! adapter remembers at most `gateway.dedup.max_entries` ids, oldest
//! forgotten first.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// The ids one adapter saw.
#[derive(Debug, Default)]
struct Seen {
    order: VecDeque<(String, Instant)>,
    at: HashMap<String, Instant>,
}

/// Every adapter's recent message ids.
#[derive(Debug, Default)]
pub(crate) struct Dedup {
    adapters: Mutex<HashMap<String, Seen>>,
}

impl Dedup {
    /// Records `(chat, message_id)` of `adapter`; `true` when it was seen
    /// within `ttl` (a duplicate). Events without an id are never
    /// duplicates.
    pub(crate) fn seen(
        &self,
        adapter: &str,
        chat: &str,
        message_id: &str,
        ttl: Duration,
        max_entries: usize,
        now: Instant,
    ) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let key = format!("{chat}\u{1f}{message_id}");
        let mut adapters = self.adapters.lock().expect("dedup poisoned");
        let seen = adapters.entry(adapter.to_owned()).or_default();
        if seen
            .at
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) <= ttl)
        {
            return true;
        }
        while let Some((old, at)) = seen.order.front() {
            let expired = now.duration_since(*at) > ttl;
            if !expired && seen.order.len() < max_entries.max(1) {
                break;
            }
            if seen.at.get(old) == Some(at) {
                seen.at.remove(old);
            }
            seen.order.pop_front();
        }
        seen.at.insert(key.clone(), now);
        seen.order.push_back((key, now));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redelivery_within_the_window_is_a_duplicate() {
        let dedup = Dedup::default();
        let ttl = Duration::from_mins(5);
        let now = Instant::now();
        assert!(!dedup.seen("default/slack", "C1", "m1", ttl, 10, now));
        assert!(dedup.seen("default/slack", "C1", "m1", ttl, 10, now));
        assert!(!dedup.seen("default/slack", "C2", "m1", ttl, 10, now));
        assert!(!dedup.seen("work/slack", "C1", "m1", ttl, 10, now));
        let later = now + Duration::from_secs(301);
        assert!(!dedup.seen("default/slack", "C1", "m1", ttl, 10, later));
    }

    #[test]
    fn the_oldest_ids_are_forgotten_beyond_the_cap() {
        let dedup = Dedup::default();
        let ttl = Duration::from_mins(5);
        let now = Instant::now();
        for id in ["a", "b", "c"] {
            assert!(!dedup.seen("x", "c", id, ttl, 2, now));
        }
        assert!(!dedup.seen("x", "c", "a", ttl, 2, now));
        assert!(dedup.seen("x", "c", "c", ttl, 2, now));
    }
}
