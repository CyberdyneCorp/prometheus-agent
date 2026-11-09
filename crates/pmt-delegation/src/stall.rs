//! Stall detection instead of wall-clock timeouts (spec:
//! subagent-delegation, Stall detection instead of wall-clock timeouts).
//!
//! A child has no default time limit. It is stalled after 450 seconds
//! without progress between iterations, or 1200 seconds inside one tool
//! call, or after `delegation.inactivity_timeout_secs` when that is set.
//! A stalled child is interrupted, given [`GRACE`] to wind down, then
//! finalised as `stalled` with the quiet time observed.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Quiet time between iterations before a child is stalled.
pub const BETWEEN_ITERATIONS: Duration = Duration::from_secs(450);
/// Time inside one tool call before a child is stalled.
pub const IN_TOOL: Duration = Duration::from_mins(20);
/// How long a stalled child gets to stop after its interrupt.
pub const GRACE: Duration = Duration::from_mins(2);
/// How often the monitor samples a child.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(15);

/// The thresholds a child is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    between: Duration,
    in_tool: Duration,
}

impl Policy {
    /// The built-in thresholds, or `inactivity_timeout_secs` for both when
    /// it is not `0`.
    #[must_use]
    pub fn new(inactivity_timeout_secs: u64) -> Self {
        match inactivity_timeout_secs {
            0 => Self {
                between: BETWEEN_ITERATIONS,
                in_tool: IN_TOOL,
            },
            secs => Self {
                between: Duration::from_secs(secs),
                in_tool: Duration::from_secs(secs),
            },
        }
    }

    /// How long `activity` has been quiet, when that makes it stalled.
    #[must_use]
    pub fn stalled(&self, activity: &Activity, now: Instant) -> Option<Duration> {
        let state = activity.lock();
        let quiet = now.saturating_duration_since(state.last_progress);
        let limit = if state.in_tool {
            self.in_tool
        } else {
            self.between
        };
        (quiet >= limit).then_some(quiet)
    }
}

/// What the monitor knows of a child.
#[derive(Debug)]
pub struct Activity {
    state: Mutex<ActivityState>,
}

#[derive(Debug, Clone, Copy)]
struct ActivityState {
    last_progress: Instant,
    in_tool: bool,
}

impl Activity {
    /// A child that just made progress.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ActivityState {
                last_progress: Instant::now(),
                in_tool: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ActivityState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The child made progress (an iteration, a tool result).
    pub fn progressed(&self) {
        let mut state = self.lock();
        state.last_progress = Instant::now();
        state.in_tool = false;
    }

    /// The child entered a tool call.
    pub fn tool_started(&self) {
        let mut state = self.lock();
        state.last_progress = Instant::now();
        state.in_tool = true;
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

/// Samples `activity` every [`SAMPLE_EVERY`] until it stalls; returns the
/// quiet time then (cancel the future when the child ends).
pub async fn watch(policy: Policy, activity: &Activity) -> Duration {
    loop {
        tokio::time::sleep(SAMPLE_EVERY).await;
        if let Some(quiet) = policy.stalled(activity, Instant::now()) {
            return quiet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Progressing child never stalls.
    #[tokio::test(start_paused = true)]
    async fn a_child_progressing_every_minute_for_two_hours_never_stalls() {
        let activity = std::sync::Arc::new(Activity::new());
        let watched = std::sync::Arc::clone(&activity);
        let monitor = tokio::spawn(async move { watch(Policy::new(0), &watched).await });
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_mins(1)).await;
            activity.progressed();
        }
        assert!(!monitor.is_finished(), "the monitor reported a stall");
        monitor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_children_stall_after_their_threshold() {
        let activity = Activity::new();
        let quiet = watch(Policy::new(0), &activity).await;
        assert!(quiet >= BETWEEN_ITERATIONS && quiet < BETWEEN_ITERATIONS + SAMPLE_EVERY);
        activity.tool_started();
        let quiet = watch(Policy::new(0), &activity).await;
        assert!(quiet >= IN_TOOL);
        activity.progressed();
        let quiet = watch(Policy::new(60), &activity).await;
        assert!(quiet >= Duration::from_mins(1) && quiet < Duration::from_secs(90));
    }
}
