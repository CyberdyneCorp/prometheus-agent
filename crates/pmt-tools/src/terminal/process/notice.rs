//! What background processes tell their session, and the session-wide cap
//! on how often (spec: execution-environments, Background notifications
//! are throttled).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Notifications a session takes per [`GLOBAL_WINDOW`].
pub const GLOBAL_CAP: usize = 15;
/// The window of [`GLOBAL_CAP`].
pub const GLOBAL_WINDOW: Duration = Duration::from_secs(10);
/// How long progress notifications stop once the cap was hit.
pub const GLOBAL_COOLDOWN: Duration = Duration::from_secs(30);

/// A finished process whose caller asked to be told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completion {
    /// Its id.
    pub process_id: String,
    /// Its command line.
    pub command: String,
    /// Its exit code (128 + signal for a signal death).
    pub exit_code: i32,
}

/// Output of a still-running process: a watched line or a heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// Its id.
    pub process_id: String,
    /// Its command line.
    pub command: String,
    /// The matching line, or what it printed since the last heartbeat
    /// (redacted).
    pub text: String,
}

/// One notification of a background process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// It exited.
    Exited(Completion),
    /// A line matched one of its `watch_patterns`.
    Matched(Progress),
    /// Its `heartbeat` came due.
    Heartbeat(Progress),
}

impl Notice {
    /// The process it is about.
    #[must_use]
    pub fn process_id(&self) -> &str {
        match self {
            Self::Exited(done) => &done.process_id,
            Self::Matched(progress) | Self::Heartbeat(progress) => &progress.process_id,
        }
    }

    /// The text a chat shows for it.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Exited(done) if done.exit_code == 0 => {
                format!(
                    "Background process {} finished: {}",
                    done.process_id, done.command
                )
            }
            Self::Exited(done) => format!(
                "Background process {} failed (exit {}): {}",
                done.process_id, done.exit_code, done.command
            ),
            Self::Matched(p) => format!(
                "Background process {} matched a watch pattern: {}",
                p.process_id, p.text
            ),
            Self::Heartbeat(p) if p.text.is_empty() => format!(
                "Background process {} is still running (no new output): {}",
                p.process_id, p.command
            ),
            Self::Heartbeat(p) => format!(
                "Background process {} is still running; new output:\n{}",
                p.process_id, p.text
            ),
        }
    }
}

/// The session-wide cap: at most [`GLOBAL_CAP`] notifications per
/// [`GLOBAL_WINDOW`]; hitting it silences progress notifications for
/// [`GLOBAL_COOLDOWN`]. Exits are counted but never dropped: a process
/// exits once, and a lost exit would leave the model waiting forever.
#[derive(Debug, Default)]
pub(super) struct Limiter {
    sent: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

impl Limiter {
    /// Whether a notice may go out at `now` (and counts it when it does).
    pub(super) fn admit(&mut self, notice: &Notice, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= GLOBAL_WINDOW)
        {
            self.sent.pop_front();
        }
        let exit = matches!(notice, Notice::Exited(_));
        if !exit {
            if self.cooldown_until.is_some_and(|until| now < until) {
                return false;
            }
            if self.sent.len() >= GLOBAL_CAP {
                self.cooldown_until = Some(now + GLOBAL_COOLDOWN);
                return false;
            }
        }
        self.sent.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress() -> Notice {
        Notice::Matched(Progress {
            process_id: "proc_1".into(),
            command: "x".into(),
            text: "hit".into(),
        })
    }

    #[test]
    fn the_session_cap_starts_a_cooldown_that_spares_exits() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for _ in 0..GLOBAL_CAP {
            assert!(limiter.admit(&progress(), start));
        }
        assert!(!limiter.admit(&progress(), start));
        let exit = Notice::Exited(Completion {
            process_id: "proc_1".into(),
            command: "x".into(),
            exit_code: 0,
        });
        assert!(limiter.admit(&exit, start));
        // The window has passed but the cooldown has not.
        assert!(!limiter.admit(&progress(), start + GLOBAL_WINDOW));
        assert!(limiter.admit(&progress(), start + GLOBAL_COOLDOWN));
    }
}
