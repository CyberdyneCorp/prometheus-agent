//! A turn a restart interrupted (spec: messaging-gateway, Discarding a
//! restart-interrupted turn). While the session's last turn is
//! `resume_pending`, new input waits behind it in arrival order until the
//! user chooses `/retry` (run it again, then the waiting input) or
//! `/discard` (give it up, its partial transcript kept, and run the
//! waiting input now); after `gateway.sessions.resume_pending_ttl_secs`
//! it is discarded automatically.

use std::time::Duration;

use jiff::Timestamp;
use tokio::time::Instant;

use super::{Lane, Origin, Pending};
use crate::messaging::attention::span;

/// What the chat is told the first time input waits behind the turn.
pub(crate) const WAITING: &str = "Your message waits behind the turn the restart interrupted: \
    reply /retry to run that turn again or /discard to drop it.";
/// What `/discard` answers.
pub(crate) const DISCARDED: &str = "Dropped the interrupted turn.";
/// What `/discard` answers with nothing pending.
pub(crate) const NOTHING_TO_DISCARD: &str = "No interrupted turn to discard.";

/// The queue is held for a `resume_pending` turn.
#[derive(Debug)]
pub(crate) struct Held {
    deadline: Instant,
    ttl: Duration,
}

impl Held {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
}

impl Lane {
    /// The hold of the current session, when its last turn is pending a
    /// resume and the hold has not expired (an expired one is discarded).
    fn pending_resume(&self, session: &str) -> Option<Held> {
        let store = self.shared.store(&self.profile)?;
        let id = session.parse().ok()?;
        let routed = store.routed_session(id).ok().flatten()?;
        if !routed.resume_pending {
            return None;
        }
        let ttl = Duration::from_secs(self.config().map_or(86_400, |s| {
            s.config().gateway.sessions.resume_pending_ttl_secs
        }));
        let since = routed.resume_at.unwrap_or_else(Timestamp::now);
        let elapsed = Timestamp::now()
            .duration_since(since)
            .unsigned_abs()
            .min(ttl);
        Some(Held {
            deadline: Instant::now() + ttl.saturating_sub(elapsed),
            ttl,
        })
    }

    /// Whether `pending` must wait behind a turn pending a resume (it is
    /// queued then).
    pub(super) fn hold(&mut self, session: &str, pending: &Pending) -> bool {
        if !matches!(pending.origin, Origin::Human | Origin::Queued) {
            return false;
        }
        if self.held.is_none() {
            self.held = self.pending_resume(session);
            if self.held.is_some() {
                self.say(WAITING);
            }
        }
        if self.held.is_none() {
            return false;
        }
        match pending.origin {
            Origin::Human => self.queue_human(pending.clone()),
            _ => self.pending.push_back(pending.clone()),
        }
        true
    }

    /// Whether the queue's head waits for the held turn.
    pub(super) fn waiting_for_resume(&self) -> bool {
        self.held.is_some()
            && self
                .pending
                .front()
                .is_some_and(|p| matches!(p.origin, Origin::Human | Origin::Queued))
    }

    /// `/discard`.
    pub(super) async fn discard_command(&mut self) {
        let session = self.current_session().unwrap_or_default();
        if self.release(&session) {
            self.say(DISCARDED);
            self.next().await;
        } else {
            self.say(NOTHING_TO_DISCARD);
        }
    }

    /// `/retry` while a turn is pending a resume: the queue is released
    /// behind the retried turn. `false` when nothing was pending.
    pub(super) fn retry_releases(&mut self) -> bool {
        let session = self.current_session().unwrap_or_default();
        self.release(&session)
    }

    /// Gives up the pending turn of `session`; `true` when one was.
    fn release(&mut self, session: &str) -> bool {
        self.held = None;
        let Some(store) = self.shared.store(&self.profile) else {
            return false;
        };
        let Ok(id) = session.parse() else {
            return false;
        };
        store.discard_resume(id).unwrap_or_else(|err| {
            tracing::warn!("the pending turn was not discarded: {err}");
            false
        })
    }

    /// Discards an expired hold and runs what waited.
    pub(super) async fn check_held(&mut self, now: Instant) {
        let Some(held) = self.held.as_ref().filter(|h| h.deadline <= now) else {
            return;
        };
        let ttl = held.ttl;
        let session = self.current_session().unwrap_or_default();
        self.release(&session);
        self.say(&format!(
            "The interrupted turn was dropped after {} without /retry.",
            span(ttl)
        ));
        self.next().await;
    }
}
