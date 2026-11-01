//! Where a turn's trajectory events go. A failing subscriber is logged at
//! DEBUG and never aborts the turn (spec: agent-turn-loop, Step events per
//! iteration).

use std::sync::{Arc, Mutex, PoisonError};

use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::writer::{Emit, TrajectoryWriter};

use super::model::BoxFuture;

/// Receives every event of a session's turns.
pub trait EventSink: Send + Sync {
    /// Records one event.
    ///
    /// # Errors
    ///
    /// The subscriber's failure (the loop logs it and carries on).
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>>;
}

/// Drops every event.
#[derive(Debug, Default)]
pub struct NoEvents;

impl EventSink for NoEvents {
    fn emit(&self, _: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        Box::pin(async { Ok(()) })
    }
}

impl EventSink for TrajectoryWriter {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        Box::pin(self.record(emit))
    }
}

/// Keeps every event in memory (tests, RPC subscribers that poll).
#[derive(Debug, Default)]
pub struct Recorded {
    events: Mutex<Vec<Emit>>,
}

impl Recorded {
    /// A shared empty recorder.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every event so far.
    #[must_use]
    pub fn events(&self) -> Vec<Event> {
        self.emits().into_iter().map(|e| e.event).collect()
    }

    /// Every event with its header.
    #[must_use]
    pub fn emits(&self) -> Vec<Emit> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl EventSink for Recorded {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(emit);
        Box::pin(async { Ok(()) })
    }
}

/// Emits `event` inside `turn` and logs a subscriber failure.
pub(crate) async fn emit(sink: &dyn EventSink, turn: &str, event: Event) {
    let kind = event.kind();
    if let Err(err) = sink.emit(Emit::now(event).in_turn(turn, None)).await {
        tracing::debug!(event = kind, error = %err, "trajectory subscriber failed; turn continues");
    }
}
