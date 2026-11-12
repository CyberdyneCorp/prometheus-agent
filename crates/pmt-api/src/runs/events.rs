//! A run's event buffer and `GET /v1/runs/{id}/events` (spec: api-server,
//! Run events stream). Every event gets the next id; a client that lost
//! its connection reconnects with `Last-Event-ID` and gets what followed.
//! A buffer nobody reads is emptied 5 minutes after its last subscriber
//! left (or, never read, after its run ended); the run itself goes on.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::http::Sse;

/// How long an unread buffer is kept.
pub(crate) const BUFFER_TTL: Duration = Duration::from_mins(5);

/// The terminal event names.
pub(crate) const TERMINAL: [&str; 4] = [
    "run.completed",
    "run.failed",
    "run.cancelled",
    "run.interrupted",
];

#[derive(Debug, Default)]
struct Log {
    events: Vec<(u64, String, Value)>,
    next: u64,
    ended: bool,
    subscribers: usize,
    /// When the last subscriber left (or the run ended unread).
    idle_since: Option<Instant>,
}

/// One run's events.
#[derive(Debug, Default)]
pub(crate) struct Events {
    log: Mutex<Log>,
    added: Notify,
}

impl Events {
    /// Appends `event` with `data`; ignored once the run ended.
    pub(crate) fn push(&self, event: &str, data: Value) {
        let mut log = self.log.lock().expect("events poisoned");
        if log.ended {
            return;
        }
        log.next += 1;
        let id = log.next;
        log.events.push((id, event.to_owned(), data));
        if TERMINAL.contains(&event) {
            log.ended = true;
            if log.subscribers == 0 {
                log.idle_since = Some(Instant::now());
            }
        }
        drop(log);
        self.added.notify_waiters();
    }

    /// Drops an unread buffer past its time; `true` when the run ended and
    /// its buffer is gone (the run can leave the registry).
    pub(crate) fn expire(&self, now: Instant) -> bool {
        let mut log = self.log.lock().expect("events poisoned");
        let idle = log.subscribers == 0
            && log
                .idle_since
                .is_some_and(|since| now.duration_since(since) >= BUFFER_TTL);
        if idle {
            log.events.clear();
        }
        idle && log.ended
    }

    /// Events after `last`, and whether the stream is over after them.
    fn after(&self, last: u64) -> (Vec<(u64, String, Value)>, bool) {
        let log = self.log.lock().expect("events poisoned");
        let events: Vec<_> = log
            .events
            .iter()
            .filter(|(id, ..)| *id > last)
            .cloned()
            .collect();
        (events, log.ended)
    }

    fn subscribe(&self) {
        let mut log = self.log.lock().expect("events poisoned");
        log.subscribers += 1;
        log.idle_since = None;
    }

    fn unsubscribe(&self) {
        let mut log = self.log.lock().expect("events poisoned");
        log.subscribers = log.subscribers.saturating_sub(1);
        if log.subscribers == 0 {
            log.idle_since = Some(Instant::now());
        }
    }
}

/// Streams `events` after `last` to `out` until the terminal event or the
/// client leaves.
pub(crate) async fn stream(
    events: std::sync::Arc<Events>,
    (run_id, session_key): (String, Option<String>),
    mut last: u64,
    out: mpsc::UnboundedSender<Sse>,
) {
    events.subscribe();
    loop {
        let added = events.added.notified();
        let (batch, ended) = events.after(last);
        let mut gone = false;
        for (id, name, mut data) in batch {
            data["event"] = name.clone().into();
            data["run_id"] = run_id.clone().into();
            if let Some(key) = &session_key {
                data["session_key"] = key.clone().into();
            }
            gone |= out
                .send(Sse {
                    id: Some(id),
                    event: Some(name),
                    data: data.to_string(),
                })
                .is_err();
            last = id;
        }
        if gone || ended {
            break;
        }
        tokio::select! {
            () = added => {}
            () = out.closed() => break,
        }
    }
    events.unsubscribe();
}

/// The single terminal event of a run whose buffer is gone.
pub(crate) fn terminal_of(run: &pmt_store::api::runs::ApiRun) -> (String, Value) {
    let event = match run.status.as_str() {
        "completed" => "run.completed",
        "cancelled" => "run.cancelled",
        "interrupted" => "run.interrupted",
        _ => "run.failed",
    };
    let parse = |v: &Option<String>| {
        v.as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .unwrap_or(Value::Null)
    };
    (
        event.to_owned(),
        json!({"status": run.status, "output": run.output, "error": run.error,
            "usage": parse(&run.usage), "runtime": parse(&run.runtime)}),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{BUFFER_TTL, Events, stream};

    /// Scenario: Reconnect without loss.
    #[tokio::test]
    async fn a_reconnect_resumes_after_the_last_seen_id() {
        let events = Arc::new(Events::default());
        for n in 0..41 {
            events.push("message.delta", json!({"delta": n}));
        }
        events.push("message.delta", json!({"delta": "forty-second"}));
        events.push("run.completed", json!({}));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        stream(Arc::clone(&events), ("run_1".into(), None), 41, tx).await;
        let first = rx.recv().await.unwrap();
        assert_eq!(first.id, Some(42));
        assert!(first.data.contains("forty-second"));
        let last = rx.recv().await.unwrap();
        assert_eq!(last.event.as_deref(), Some("run.completed"));
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn an_unread_buffer_expires_without_ending_the_run() {
        let events = Events::default();
        events.push("run.started", json!({}));
        events.subscribe();
        events.unsubscribe();
        let later = Instant::now() + BUFFER_TTL + Duration::from_secs(1);
        assert!(!events.expire(later), "a running run stays registered");
        assert!(events.after(0).0.is_empty());
        events.push("run.completed", json!({}));
        let (left, ended) = events.after(0);
        assert!(ended);
        assert_eq!(left.len(), 1);
    }
}
