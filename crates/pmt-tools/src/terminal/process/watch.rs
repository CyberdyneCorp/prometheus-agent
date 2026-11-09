//! `watch_patterns` of one background process (spec: execution-
//! environments, Background notifications are throttled).
//!
//! A matching line is delivered at most once per [`WINDOW`]; a match inside
//! the window is throttled. After [`MAX_DELIVERIES`] deliveries, or once
//! [`MAX_THROTTLED_WINDOWS`] windows in a row throttled a match, watching
//! stops and the process falls back to a completion notification.

use std::time::{Duration, Instant};

/// Least time between two deliveries of one process.
pub const WINDOW: Duration = Duration::from_secs(15);
/// Deliveries before watching stops.
pub const MAX_DELIVERIES: u32 = 8;
/// Consecutive throttled windows before watching stops.
pub const MAX_THROTTLED_WINDOWS: u32 = 3;
/// Shortest heartbeat interval.
pub const MIN_HEARTBEAT: Duration = Duration::from_mins(1);
/// Characters of a matching line a notification carries.
const LINE_CHARS: usize = 500;

/// What one output line did to the watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Deliver this line.
    Deliver(String),
    /// Deliver this line; it was the last one: watching stops.
    DeliverLast(String),
    /// Watching just stopped: notify on completion instead.
    Stop,
}

/// The watch of one process.
#[derive(Debug, Default)]
pub(super) struct Watch {
    patterns: Vec<String>,
    /// The incomplete last line.
    carry: String,
    last_delivery: Option<Instant>,
    deliveries: u32,
    throttled_windows: u32,
    /// The current window throttled a match.
    window_throttled: bool,
    stopped: bool,
}

impl Watch {
    pub(super) fn new(patterns: Vec<String>) -> Self {
        let patterns: Vec<String> = patterns.into_iter().filter(|p| !p.is_empty()).collect();
        Self {
            stopped: patterns.is_empty(),
            patterns,
            ..Self::default()
        }
    }

    /// Feeds newly printed text; returns what the complete lines in it
    /// call for, in order (at most one stop).
    pub(super) fn feed(&mut self, text: &str, now: Instant) -> Vec<Verdict> {
        if self.stopped {
            return Vec::new();
        }
        self.carry.push_str(text);
        let Some(end) = self.carry.rfind('\n') else {
            return Vec::new();
        };
        let complete: String = self.carry.drain(..=end).collect();
        let mut verdicts = Vec::new();
        for line in complete.lines() {
            if self.stopped {
                break;
            }
            if self.patterns.iter().any(|p| line.contains(p.as_str())) {
                verdicts.extend(self.on_match(line, now));
            }
        }
        verdicts
    }

    fn on_match(&mut self, line: &str, now: Instant) -> Option<Verdict> {
        let in_window = self
            .last_delivery
            .is_some_and(|at| now.saturating_duration_since(at) < WINDOW);
        if in_window {
            if self.window_throttled {
                return None;
            }
            self.window_throttled = true;
            self.throttled_windows += 1;
            if self.throttled_windows >= MAX_THROTTLED_WINDOWS {
                self.stopped = true;
                return Some(Verdict::Stop);
            }
            return None;
        }
        if !self.window_throttled {
            self.throttled_windows = 0;
        }
        self.window_throttled = false;
        self.last_delivery = Some(now);
        self.deliveries += 1;
        let line: String = line.chars().take(LINE_CHARS).collect();
        if self.deliveries >= MAX_DELIVERIES {
            self.stopped = true;
            return Some(Verdict::DeliverLast(line));
        }
        Some(Verdict::Deliver(line))
    }
}

/// The heartbeat interval of a `heartbeat` argument: none for 0 or none,
/// else at least [`MIN_HEARTBEAT`].
#[must_use]
pub fn heartbeat_interval(seconds: Option<f64>) -> Option<Duration> {
    let seconds = seconds.filter(|s| *s > 0.0 && s.is_finite())?;
    Some(Duration::from_secs_f64(seconds).max(MIN_HEARTBEAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch() -> Watch {
        Watch::new(vec!["ERROR".into()])
    }

    #[test]
    fn matches_inside_the_window_are_throttled_and_three_throttled_windows_stop_watching() {
        let mut watch = watch();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        assert_eq!(
            watch.feed("ERROR a\n", at(0)),
            [Verdict::Deliver("ERROR a".into())]
        );
        assert!(watch.feed("ERROR b\nERROR c\n", at(1)).is_empty());
        assert_eq!(watch.feed("ERROR d\n", at(16)).len(), 1);
        assert!(watch.feed("ERROR e\n", at(17)).is_empty());
        assert_eq!(watch.feed("ERROR f\n", at(32)).len(), 1);
        assert_eq!(watch.feed("ERROR g\n", at(33)), [Verdict::Stop]);
        assert!(watch.feed("ERROR h\n", at(100)).is_empty());
    }

    #[test]
    fn a_quiet_window_resets_the_throttle_count() {
        let mut watch = watch();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        for secs in [0, 16] {
            assert_eq!(watch.feed("ERROR\n", at(secs)).len(), 1);
            assert!(watch.feed("ERROR\n", at(secs + 1)).is_empty());
        }
        // The window after this delivery throttles nothing.
        assert_eq!(watch.feed("ERROR\n", at(32)).len(), 1);
        for secs in [60, 80] {
            assert_eq!(watch.feed("ERROR\n", at(secs)).len(), 1);
            assert!(watch.feed("ERROR\n", at(secs + 1)).is_empty());
        }
        assert_eq!(watch.feed("ERROR\n", at(100)).len(), 1);
    }

    #[test]
    fn partial_lines_wait_for_their_end_and_heartbeats_are_clamped() {
        let mut watch = watch();
        let now = Instant::now();
        assert!(watch.feed("ERR", now).is_empty());
        assert_eq!(
            watch.feed("OR here\n", now),
            [Verdict::Deliver("ERROR here".into())]
        );
        assert_eq!(heartbeat_interval(Some(5.0)), Some(MIN_HEARTBEAT));
        assert_eq!(
            heartbeat_interval(Some(90.0)),
            Some(Duration::from_secs(90))
        );
        assert_eq!(heartbeat_interval(Some(0.0)), None);
        assert_eq!(heartbeat_interval(None), None);
    }
}
