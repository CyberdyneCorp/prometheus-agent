//! Webhook event coalescing (spec: platform-adapters, Webhook event
//! coalescing): a route with `coalesce = {key, window_secs,
//! max_wait_secs}` holds events whose key resolves to the same value.
//! Each newer event replaces the pending one and restarts the quiet
//! window; one turn runs on the newest when the window elapses, or
//! `max_wait_secs` after the group's first event so a steady stream
//! cannot starve it. Pending groups are flushed (not dropped) when the
//! adapter stops.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::delivery::GithubThread;

/// The newest event of a group, ready to become a turn.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub route: String,
    pub delivery: String,
    pub text: String,
    pub github: Option<GithubThread>,
}

/// Starts the turn of a settled group: the newest event and how many
/// earlier events it superseded.
pub(crate) type Settle =
    Arc<dyn Fn(Pending, usize) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct Group {
    latest: Pending,
    first_at: Instant,
    superseded: usize,
    /// Bumped by each event: a timer fires only for the event that set it.
    generation: u64,
}

type Groups = Arc<Mutex<HashMap<String, Group>>>;

/// The held groups of one adapter.
#[derive(Default)]
pub(crate) struct Coalescer {
    groups: Groups,
}

/// A group's quiet window and its cap.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub quiet: Duration,
    pub max_wait: Duration,
}

impl Coalescer {
    /// Holds `event` in group `key`, replacing its pending event; the
    /// group settles through `settle` once quiet.
    pub(crate) fn hold(&self, key: String, event: Pending, window: Window, settle: Settle) {
        let now = Instant::now();
        let (generation, deadline) = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            let group = groups.entry(key.clone()).or_insert_with(|| Group {
                latest: event.clone(),
                first_at: now,
                superseded: 0,
                generation: 0,
            });
            if group.generation > 0 {
                group.superseded += 1;
                tracing::info!(
                    route = event.route,
                    superseded = group.latest.delivery,
                    by = event.delivery,
                    "coalesced webhook delivery superseded"
                );
            }
            group.latest = event;
            group.generation += 1;
            let cap = group.first_at + window.max_wait;
            (group.generation, (now + window.quiet).min(cap))
        };
        let groups = Arc::clone(&self.groups);
        pmt_core::spawn::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let settled = {
                let mut groups = groups.lock().unwrap_or_else(PoisonError::into_inner);
                match groups.get(&key) {
                    Some(group) if group.generation == generation => groups.remove(&key),
                    _ => None,
                }
            };
            if let Some(group) = settled {
                settle(group.latest, group.superseded).await;
            }
        });
    }

    /// Settles every pending group now (the adapter is stopping).
    pub(crate) async fn flush(&self, settle: &Settle) {
        let pending: Vec<Group> = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            groups.drain().map(|(_, g)| g).collect()
        };
        for group in pending {
            settle(group.latest, group.superseded).await;
        }
    }
}

/// The turn input of a group's newest event.
pub(crate) fn noted(text: &str, superseded: usize) -> String {
    if superseded == 0 {
        return text.to_owned();
    }
    let events = if superseded == 1 { "event" } else { "events" };
    format!(
        "{text}\n\n[Coalesced: {superseded} earlier {events} for this item arrived in quick succession and were superseded by this one.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(delivery: &str) -> Pending {
        Pending {
            route: "ci".into(),
            delivery: delivery.into(),
            text: format!("push {delivery}"),
            github: None,
        }
    }

    type Seen = Arc<Mutex<Vec<(String, usize)>>>;

    fn recorder() -> (Settle, Seen) {
        let seen: Seen = Arc::default();
        let sink = Arc::clone(&seen);
        let settle: Settle = Arc::new(move |p: Pending, n| {
            sink.lock().unwrap().push((p.delivery, n));
            Box::pin(async {})
        });
        (settle, seen)
    }

    const WINDOW: Window = Window {
        quiet: Duration::from_secs(30),
        max_wait: Duration::from_mins(5),
    };

    #[tokio::test(start_paused = true)]
    async fn a_burst_runs_once_on_the_newest_event() {
        let (settle, seen) = recorder();
        let coalescer = Coalescer::default();
        for n in 1..=5 {
            coalescer.hold(
                "ci|main".into(),
                event(&format!("d{n}")),
                WINDOW,
                settle.clone(),
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        assert!(seen.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(*seen.lock().unwrap(), [("d5".to_owned(), 4)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_settles_at_max_wait() {
        let (settle, seen) = recorder();
        let coalescer = Coalescer::default();
        let window = Window {
            quiet: Duration::from_secs(30),
            max_wait: Duration::from_secs(65),
        };
        for n in 0..10 {
            coalescer.hold("k".into(), event(&format!("d{n}")), window, settle.clone());
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.first().map(|(d, n)| (d.as_str(), *n)), Some(("d6", 6)));
    }

    #[tokio::test(start_paused = true)]
    async fn flushing_settles_pending_groups() {
        let (settle, seen) = recorder();
        let coalescer = Coalescer::default();
        coalescer.hold("a".into(), event("d1"), WINDOW, settle.clone());
        coalescer.hold("b".into(), event("d2"), WINDOW, settle.clone());
        coalescer.flush(&settle).await;
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, [("d1".to_owned(), 0), ("d2".to_owned(), 0)]);
    }

    #[test]
    fn the_note_counts_superseded_events() {
        assert_eq!(noted("x", 0), "x");
        assert!(noted("x", 4).contains("4 earlier events"));
    }
}
