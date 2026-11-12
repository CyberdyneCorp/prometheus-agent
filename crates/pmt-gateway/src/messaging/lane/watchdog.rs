//! The session stall watchdog (spec: messaging-gateway, Session stall
//! watchdog and reconnect attention). A running turn that produced no
//! model output, tool progress or tool result for
//! `gateway.session_stall_timeout_secs` gets one no-progress notice in its
//! chat (and a WARN naming the session); after twice that time it is
//! interrupted, the chat is told, and the turn ends with exit reason
//! `stalled` as far as the gateway reports it (hooks, reactions).

use std::time::Duration;

use pmt_store::gateway::outbox::OutboundKind;
use serde_json::Value;
use tokio::time::Instant;

use super::Lane;
use crate::messaging::attention::span;

/// Trajectory events that count as progress.
const PROGRESS: &[&str] = &[
    "assistant.message",
    "assistant.reasoning",
    "model.call",
    "tool.call",
    "tool.result",
    "tool.batch_planned",
    "subagent.progress",
    "turn.step",
];

/// The exit reason a turn the watchdog interrupted ends with.
pub(crate) const STALLED: &str = "stalled";

/// What the chat is told when a turn shows no progress.
pub(crate) fn no_progress(strings: &pmt_i18n::Strings, timeout: Duration) -> String {
    pmt_i18n::t!(
        strings,
        "gateway.no_progress",
        span = span(strings, timeout)
    )
}

/// What the chat is told when the watchdog interrupts the turn.
pub(crate) fn interrupted(strings: &pmt_i18n::Strings, timeout: Duration) -> String {
    let span = span(strings, timeout * 2);
    pmt_i18n::t!(strings, "gateway.stalled_interrupted", span = span)
}

/// The watchdog's view of a running turn.
#[derive(Debug)]
pub(crate) struct Watch {
    progress_at: Instant,
    told: bool,
    /// The watchdog interrupted the turn.
    pub(super) stalled: bool,
}

impl Watch {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            progress_at: now,
            told: false,
            stalled: false,
        }
    }
}

impl Lane {
    /// The configured stall timeout; `None` when the watchdog is off.
    fn stall_timeout(&self) -> Option<Duration> {
        let secs = self
            .config()
            .map_or(900, |s| s.config().gateway.session_stall_timeout_secs.get());
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    /// The running turn showed progress.
    pub(super) fn progressed(&mut self) {
        if let Some(running) = self.running.as_mut() {
            running.watch.progress_at = Instant::now();
            running.watch.told = false;
        }
    }

    /// A trajectory event of the session.
    pub(super) async fn session_event(&mut self, kind: &str, payload: &Value) {
        if PROGRESS.contains(&kind) {
            self.progressed();
        }
        if kind == "turn.waiting" {
            // Waiting for a daemon-wide slot is not a stall.
            self.progressed();
            let ahead = payload["ahead"].as_u64().unwrap_or(1);
            self.say(&crate::slots::waiting_notice(
                usize::try_from(ahead).unwrap_or(1),
            ));
            return;
        }
        self.show_progress(kind, payload).await;
    }

    /// When the watchdog looks at the running turn next.
    pub(super) fn stall_due(&self) -> Option<Instant> {
        let watch = &self.running.as_ref()?.watch;
        if watch.stalled {
            return None;
        }
        let timeout = self.stall_timeout()?;
        let factor = if watch.told { 2 } else { 1 };
        Some(watch.progress_at + timeout * factor)
    }

    /// Notices, then interrupts, a turn without progress.
    pub(super) async fn check_stall(&mut self, now: Instant) {
        let (Some(timeout), Some(due)) = (self.stall_timeout(), self.stall_due()) else {
            return;
        };
        if due > now {
            return;
        }
        let session = self.session.clone().unwrap_or_default();
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.watch.told {
            running.watch.stalled = true;
            running.stopped = true;
            tracing::warn!(session_id = session, "interrupting a stalled turn");
            self.interrupt().await;
            let text = interrupted(&self.strings(), timeout);
            self.say_as(OutboundKind::Warning, &text, None);
        } else {
            running.watch.told = true;
            tracing::warn!(
                session_id = session,
                timeout_secs = timeout.as_secs(),
                "the turn shows no progress"
            );
            let text = no_progress(&self.strings(), timeout);
            self.say_as(OutboundKind::Warning, &text, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_minutes_without_progress() {
        let timeout = Duration::from_mins(5);
        let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
        assert_eq!(
            no_progress(&english, timeout),
            "No progress for 5 min; send /stop to cancel"
        );
        assert_eq!(
            interrupted(&english, timeout),
            "The turn was interrupted as stalled: no progress for 10 min."
        );
    }
}
