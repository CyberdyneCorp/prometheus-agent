//! The serialized background queue of one session's provider: jobs run
//! one at a time, in order, off the turn; a failing job is logged and the
//! next one runs.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Mirrored, ProviderBackend};

/// One background call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// `on-turn-start`.
    TurnStart(String),
    /// `sync-turn`.
    Sync {
        /// The user's text.
        user: String,
        /// The reply.
        assistant: String,
    },
    /// `queue-prefetch`.
    QueuePrefetch(String),
    /// `on-memory-write`.
    Write(Mirrored),
    /// `on-delegation`.
    Delegation(String),
}

impl Job {
    fn run(&self, backend: &dyn ProviderBackend) -> (&'static str, Result<(), String>) {
        match self {
            Self::TurnStart(text) => ("on-turn-start", backend.turn_start(text)),
            Self::Sync { user, assistant } => ("sync-turn", backend.sync_turn(user, assistant)),
            Self::QueuePrefetch(query) => ("queue-prefetch", backend.queue_prefetch(query)),
            Self::Write(write) => ("on-memory-write", backend.memory_write(write)),
            Self::Delegation(task) => ("on-delegation", backend.delegation(task)),
        }
    }
}

/// The queue and its worker.
pub struct Queue {
    tx: Mutex<Option<mpsc::UnboundedSender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Queue {
    /// Starts the worker (inside a tokio runtime).
    #[must_use]
    pub fn start(backend: Arc<dyn ProviderBackend>, name: String) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let worker = pmt_core::spawn::spawn(async move {
            while let Some(job) = rx.recv().await {
                let backend = Arc::clone(&backend);
                let ran = pmt_core::spawn::blocking(move || {
                    let (hook, result) = job.run(&*backend);
                    result.map_err(|err| format!("{hook}: {err}"))
                })
                .await;
                match ran {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        tracing::warn!(provider = %name, "memory provider call failed: {err}");
                    }
                    Err(err) => {
                        tracing::warn!(provider = %name, "memory provider call panicked: {err}");
                    }
                }
            }
        });
        Self {
            tx: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Queues `job` (dropped after the queue was drained).
    pub fn push(&self, job: Job) {
        if let Some(tx) = self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(job);
        }
    }

    /// Stops taking jobs and waits for the queued ones, for at most
    /// `limit`; what is left after it is abandoned.
    pub async fn drain(&self, limit: Duration) {
        drop(
            self.tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker
            && tokio::time::timeout(limit, worker).await.is_err()
        {
            tracing::warn!(
                "memory provider queue not drained within {} ms",
                limit.as_millis()
            );
        }
    }
}
