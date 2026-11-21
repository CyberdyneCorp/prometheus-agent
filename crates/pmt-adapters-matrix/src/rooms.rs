//! Which rooms the bot reads and joins, how a room maps onto sessions,
//! and what a skewed clock does (spec: platform-adapters, Matrix rooms,
//! invites and clock skew). `allowed_rooms` (room ids) is applied before
//! any other gating, two-member rooms (DMs) exempt; invites follow
//! `auto_join`; `session_scope` folds threads into the room or opens one
//! per top-level message; and a live event older than the adapter's start
//! minus 5 seconds is dropped, with one WARN naming clock skew once more
//! than 10 are dropped later than 30 seconds after the start.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use jiff::{SignedDuration, Timestamp};
use pmt_config::platforms::{MatrixAutoJoin, MatrixOptions, MatrixSessionScope};
use pmt_gateway::messaging::Inbound;

/// How far before the start an event may be and still count as live.
const GRACE: SignedDuration = SignedDuration::from_secs(5);
/// Drops in the first 30 seconds are the backlog, not skew.
const SETTLE: SignedDuration = SignedDuration::from_secs(30);
/// Late drops tolerated before the WARN.
const SKEW_WARN_AFTER: usize = 10;

/// The room policy of one adapter.
#[derive(Debug)]
pub(crate) struct Rooms {
    allowed: Vec<String>,
    auto_join: MatrixAutoJoin,
    scope: MatrixSessionScope,
    started: Timestamp,
    late_drops: AtomicUsize,
    warned: AtomicBool,
}

impl Rooms {
    pub(crate) fn new(options: &MatrixOptions, started: Timestamp) -> Self {
        Self {
            allowed: options.allowed_rooms.clone(),
            auto_join: options.auto_join,
            scope: options.session_scope,
            started,
            late_drops: AtomicUsize::new(0),
            warned: AtomicBool::new(false),
        }
    }

    /// Whether the bot reads `room` (of `members` joined members).
    pub(crate) fn reads(&self, room: &str, members: usize) -> bool {
        self.allowed.is_empty() || members == 2 || self.allowed.iter().any(|r| r == room)
    }

    /// Whether to accept an invite from `inviter`.
    pub(crate) fn joins(&self, inviter: &str, allow_users: &[String]) -> bool {
        match self.auto_join {
            MatrixAutoJoin::Authorized => allow_users.iter().any(|u| u == inviter),
            MatrixAutoJoin::All => true,
            MatrixAutoJoin::None => false,
        }
    }

    /// `inbound` with its session scope applied.
    pub(crate) fn scoped(&self, mut inbound: Inbound) -> Inbound {
        match self.scope {
            MatrixSessionScope::Auto => {}
            MatrixSessionScope::Room => inbound.thread_id = None,
            MatrixSessionScope::Thread => {
                if inbound.thread_id.is_none() {
                    inbound.thread_id = Some(inbound.message_id.clone());
                }
            }
        }
        inbound
    }

    /// Whether an event sent at `sent` (seen at `now`) is too old to answer;
    /// counts the late drops and warns once about clock skew.
    pub(crate) fn stale(&self, sent: Timestamp, now: Timestamp) -> bool {
        if sent >= self.started - GRACE {
            return false;
        }
        if now > self.started + SETTLE {
            let late = self.late_drops.fetch_add(1, Ordering::Relaxed) + 1;
            if late > SKEW_WARN_AFTER && !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    platform = crate::PLATFORM,
                    dropped = late,
                    "dropped {late} Matrix events older than the adapter's start; the clock of this host or of the homeserver is likely skewed"
                );
            }
        }
        true
    }

    /// Late drops so far (tests).
    #[cfg(test)]
    fn late_drops(&self) -> usize {
        self.late_drops.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use pmt_gateway::messaging::ChatType;

    use super::*;

    /// Options from the JSON object members `fields`.
    fn rooms(fields: &str) -> Rooms {
        let options: MatrixOptions = serde_json::from_str(&format!("{{{fields}}}")).unwrap();
        Rooms::new(&options, Timestamp::from_second(1_000_000).unwrap())
    }

    #[test]
    fn allowed_rooms_gate_groups_but_not_dms() {
        let r = rooms(r#""allowed_rooms": ["!ops:hs"]"#);
        assert!(r.reads("!ops:hs", 7));
        assert!(!r.reads("!random:hs", 7));
        assert!(r.reads("!dm:hs", 2));
        assert!(rooms("").reads("!random:hs", 7));
    }

    #[test]
    fn invites_follow_auto_join() {
        let allow = vec!["@ana:hs".to_owned()];
        assert!(rooms("").joins("@ana:hs", &allow));
        assert!(!rooms("").joins("@eve:hs", &allow));
        assert!(rooms(r#""auto_join": "all""#).joins("@eve:hs", &allow));
        assert!(!rooms(r#""auto_join": "none""#).joins("@ana:hs", &allow));
    }

    #[test]
    fn session_scope_folds_or_opens_threads() {
        let mut event = Inbound::text(ChatType::Group, "!ops:hs", "@ana:hs", "hi");
        event.message_id = "$top".into();
        assert_eq!(
            rooms(r#""session_scope": "thread""#)
                .scoped(event.clone())
                .thread_id
                .as_deref(),
            Some("$top")
        );
        event.thread_id = Some("$root".into());
        assert_eq!(
            rooms(r#""session_scope": "room""#)
                .scoped(event.clone())
                .thread_id,
            None
        );
        assert_eq!(rooms("").scoped(event).thread_id.as_deref(), Some("$root"));
    }

    #[test]
    fn old_events_are_dropped_and_late_drops_counted() {
        let r = rooms("");
        let start = Timestamp::from_second(1_000_000).unwrap();
        let at = |s: i64| Timestamp::from_second(s).unwrap();
        assert!(!r.stale(at(999_996), at(1_000_001)), "within the grace");
        assert!(
            r.stale(at(999_990), at(1_000_001)),
            "backlog before the start"
        );
        assert_eq!(r.late_drops(), 0, "early drops are the backlog");
        for _ in 0..11 {
            assert!(r.stale(at(999_000), start + SignedDuration::from_mins(5)));
        }
        assert_eq!(r.late_drops(), 11);
        assert!(r.warned.load(Ordering::Relaxed));
    }
}
