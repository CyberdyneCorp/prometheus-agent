//! The daemon-level event stream of `/api/events` (spec: web-dashboard,
//! Live event stream): what the dashboard shows outside an open session.
//! A hub tap turns session events, server requests and daemon broadcasts
//! into a few dashboard events, each with a `seq`; the newest
//! [`RETAINED`] are kept so a reconnect with `since=<seq>` replays what it
//! missed, or learns it is `truncated`.

use std::collections::VecDeque;
use std::sync::Mutex;

use pmt_gateway::hub::Tapped;
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Events kept for replay.
pub(crate) const RETAINED: usize = 1024;

/// One dashboard event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub seq: u64,
    pub profile: String,
    pub frame: Value,
}

/// What a reconnect gets before live events.
#[derive(Debug, PartialEq)]
pub(crate) struct Replay {
    pub events: Vec<Event>,
    /// Events after `since` were evicted before this reconnect.
    pub truncated: bool,
}

#[derive(Debug, Default)]
struct Log {
    next: u64,
    kept: VecDeque<Event>,
}

/// The stream every `/api/events` socket follows.
#[derive(Debug)]
pub(crate) struct Feed {
    log: Mutex<Log>,
    live: broadcast::Sender<Event>,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            log: Mutex::default(),
            live: broadcast::channel(RETAINED).0,
        }
    }
}

/// The dashboard event an event the hub sent maps to, if any.
fn classify(tapped: &Tapped<'_>) -> Option<(&'static str, Value)> {
    let data = tapped.data;
    let session = tapped.session_id.map(str::to_owned);
    let named = match tapped.event {
        "session.event" => match data["type"].as_str()? {
            "session.start" if data["payload"]["source"] == "cron" => "cron.run",
            "session.start" => "session.started",
            "session.end" => "session.ended",
            "budget.warning" | "budget.exceeded" => "budget.threshold",
            _ => return None,
        },
        "turn.finished" => "turn.finished",
        "approval.request" => "approval.request",
        "request.resolved" | "request.cancel" => "request.settled",
        "adapter.state" => "adapter.state",
        _ => return None,
    };
    let detail = match tapped.event {
        "session.event" => data["payload"].clone(),
        _ => data.clone(),
    };
    Some((named, json!({"session_id": session, "data": detail})))
}

impl Feed {
    /// Records an event the hub sent when the dashboard shows it.
    pub(crate) fn tap(&self, tapped: &Tapped<'_>) {
        if let Some((event, frame)) = classify(tapped) {
            self.record(tapped.profile, event, frame);
        }
    }

    /// Records a dashboard event of its own (an adapter pairing step):
    /// `data` goes out as the frame's `data`.
    pub(crate) fn publish(&self, profile: &str, event: &'static str, data: Value) {
        let mut frame = json!({"session_id": null});
        frame["data"] = data;
        self.record(profile, event, frame);
    }

    fn record(&self, profile: &str, event: &'static str, mut frame: Value) {
        let mut log = self.log.lock().expect("feed poisoned");
        log.next += 1;
        let seq = log.next;
        frame["seq"] = seq.into();
        frame["event"] = event.into();
        frame["profile"] = profile.into();
        let event = Event {
            seq,
            profile: profile.to_owned(),
            frame,
        };
        if log.kept.len() == RETAINED {
            log.kept.pop_front();
        }
        log.kept.push_back(event.clone());
        // Sent under the lock, so live events follow a replay in order.
        let _ = self.live.send(event);
    }

    /// Subscribes to live events, with what came after `since` (`None`:
    /// nothing replayed).
    pub(crate) fn follow(&self, since: Option<u64>) -> (Replay, broadcast::Receiver<Event>) {
        let log = self.log.lock().expect("feed poisoned");
        let receiver = self.live.subscribe();
        let Some(since) = since else {
            return (
                Replay {
                    events: Vec::new(),
                    truncated: false,
                },
                receiver,
            );
        };
        let oldest = log.kept.front().map_or(log.next + 1, |e| e.seq);
        let replay = Replay {
            events: log.kept.iter().filter(|e| e.seq > since).cloned().collect(),
            truncated: since + 1 < oldest && since < log.next,
        };
        (replay, receiver)
    }
}

#[cfg(test)]
mod tests {
    use pmt_gateway::hub::Tapped;
    use serde_json::{Value, json};

    use super::{Feed, RETAINED};

    fn tap(feed: &Feed, profile: &str, event: &str, data: &Value) {
        feed.tap(&Tapped {
            profile,
            session_id: Some("ses_1"),
            event,
            data,
        });
    }

    /// Scenario: Approval badge (the stream side).
    #[test]
    fn an_approval_request_becomes_a_dashboard_event_with_its_profile() {
        let feed = Feed::default();
        let (_, mut live) = feed.follow(None);
        tap(
            &feed,
            "work",
            "approval.request",
            &json!({"id": "srq-1", "command": "rm"}),
        );
        tap(
            &feed,
            "work",
            "session.event",
            &json!({"type": "assistant.delta", "payload": {}}),
        );
        let event = live.try_recv().unwrap();
        assert_eq!(event.profile, "work");
        assert_eq!(event.frame["event"], "approval.request");
        assert_eq!(event.frame["session_id"], "ses_1");
        assert_eq!(event.frame["data"]["id"], "srq-1");
        assert_eq!(event.frame["seq"], 1);
        assert!(live.try_recv().is_err(), "deltas are not dashboard events");
    }

    #[test]
    fn a_reconnect_replays_what_it_missed_or_says_truncated() {
        let feed = Feed::default();
        let start = json!({"type": "session.start", "payload": {"source": "cli"}});
        for _ in 0..3 {
            tap(&feed, "default", "session.event", &start);
        }
        let (replay, _) = feed.follow(Some(1));
        assert_eq!(
            replay.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(!replay.truncated);
        for _ in 0..RETAINED {
            tap(&feed, "default", "session.event", &start);
        }
        let (replay, _) = feed.follow(Some(1));
        assert!(replay.truncated);
        let (current, _) = feed.follow(Some(3 + RETAINED as u64));
        assert!(current.events.is_empty() && !current.truncated);
    }
}
