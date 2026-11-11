//! Live child progress (spec: subagent-delegation, Live child progress
//! streaming).
//!
//! A child's tool calls and iterations update its [`Snapshot`]; a pacer
//! publishes at most [`PER_SECOND`] `subagent.progress` events per second
//! per child, always carrying the latest state, so a burst of calls is
//! coalesced and the last event reflects the last call. The events go to
//! the root session's event stream and trajectory only, never into a
//! model's context.

use std::time::Duration;

use pmt_core::subagent_tree::one_line;
use pmt_obs::trajectory::payload::{SubagentProgress, Usage};
use tokio::sync::watch;

/// Most progress events per second per child.
pub const PER_SECOND: u32 = 4;

/// The minimum gap between two events of one child.
#[must_use]
pub fn interval() -> Duration {
    Duration::from_secs(1) / PER_SECOND
}

/// A child's current state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Model calls so far.
    pub iteration: u32,
    /// The tool it runs, if any.
    pub tool: Option<String>,
    /// One line of what it does.
    pub activity: String,
    /// Tool calls so far.
    pub tool_calls: u32,
    /// Cumulative usage.
    pub usage: Usage,
}

/// Who a child is, for its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// The spawning session or subagent.
    pub parent_id: String,
    /// Depth from 1.
    pub depth: u32,
}

impl Identity {
    /// The event payload of `snapshot`.
    #[must_use]
    pub fn event(&self, snapshot: &Snapshot) -> SubagentProgress {
        SubagentProgress {
            subagent_id: self.subagent_id.clone(),
            parent_id: self.parent_id.clone(),
            depth: self.depth,
            iteration: snapshot.iteration,
            tool: snapshot.tool.clone(),
            activity: one_line(&snapshot.activity),
            usage: snapshot.usage.clone(),
        }
    }
}

/// Publishes `updates` through `publish`, at most [`PER_SECOND`] per
/// second, the latest state winning; returns when the sender is dropped
/// (after publishing a final pending state).
pub async fn pace<F>(mut updates: watch::Receiver<Snapshot>, mut publish: F)
where
    F: FnMut(Snapshot) + Send,
{
    loop {
        if updates.changed().await.is_err() {
            return;
        }
        let snapshot = updates.borrow_and_update().clone();
        publish(snapshot);
        tokio::time::sleep(interval()).await;
        if updates.has_changed().is_err() {
            // Sender gone: whatever arrived during the pause is the final
            // state.
            let last = updates.borrow().clone();
            publish(last);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Scenario: Progress coalesced under load.
    #[tokio::test(start_paused = true)]
    async fn forty_calls_in_one_second_make_at_most_four_events_ending_on_the_last() {
        let (tx, rx) = watch::channel(Snapshot::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let start = tokio::time::Instant::now();
        let pacer = pmt_core::spawn::spawn(pace(rx, move |s: Snapshot| {
            sink.lock().unwrap().push((start.elapsed(), s.tool_calls));
        }));
        for call in 1..=40u32 {
            tx.send_modify(|s| {
                s.tool_calls = call;
                s.tool = Some("read_file".into());
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(tx);
        pacer.await.unwrap();
        let seen = seen.lock().unwrap().clone();
        let first_second = seen
            .iter()
            .filter(|(at, _)| *at < Duration::from_secs(1))
            .count();
        assert!(first_second <= 4, "{seen:?}");
        assert_eq!(seen.last().unwrap().1, 40, "{seen:?}");
    }
}
