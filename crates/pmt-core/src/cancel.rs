//! The cancellation token tree (spec: workspace-architecture, Cancellation
//! latency).
//!
//! The process owns a root token; each session, turn, model stream and tool
//! call takes a child of its parent's token. Cancelling a node cancels its
//! whole subtree at once and leaves parents and siblings running, so `/stop`
//! on one turn never touches another session. Work observes cancellation by
//! racing against [`CancelToken::cancelled`] (or wrapping itself in
//! [`CancelToken::run`]), which wakes immediately instead of polling, well
//! inside the 500 ms bound.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::{Error, codes};

/// How long a running turn may take to stop after an interrupt.
pub const CANCEL_LATENCY: Duration = Duration::from_millis(500);

/// A node of the cancellation tree. Clones share the node.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(CancellationToken);

impl CancelToken {
    /// A new root token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A child token: cancelled with this token, cancellable on its own.
    #[must_use]
    pub fn child(&self) -> Self {
        Self(self.0.child_token())
    }

    /// Cancels this token and every descendant.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Whether this token (or an ancestor) was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Completes once the token is cancelled.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }

    /// Runs `work` until it finishes or the token is cancelled; on
    /// cancellation `work` is dropped (which stops it) and `task.cancelled`
    /// is returned.
    ///
    /// # Errors
    ///
    /// `task.cancelled` (kind `cancelled`) when the token fired first.
    pub async fn run<F: Future>(&self, work: F) -> Result<F::Output, Error> {
        tokio::select! {
            biased;
            () = self.0.cancelled() => {
                Err(Error::new(&codes::TASK_CANCELLED, "cancelled"))
            }
            output = work => Ok(output),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CANCEL_LATENCY, CancelToken};
    use crate::error::ErrorKind;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_the_root_stops_a_deep_task_within_the_bound() {
        let root = CancelToken::new();
        let tool = root.child().child().child(); // session > turn > tool call
        let task =
            tokio::spawn(
                async move { tool.run(tokio::time::sleep(Duration::from_mins(10))).await },
            );
        tokio::time::sleep(Duration::from_millis(20)).await;

        let interrupted = Instant::now();
        root.cancel();
        let outcome = task.await.unwrap();
        let latency = interrupted.elapsed();

        assert_eq!(outcome.unwrap_err().kind(), ErrorKind::Cancelled);
        assert!(latency < CANCEL_LATENCY, "took {latency:?}");
    }

    #[tokio::test]
    async fn cancelling_a_child_leaves_parent_and_siblings_running() {
        let session = CancelToken::new();
        let turn_a = session.child();
        let turn_b = session.child();
        turn_a.cancel();
        assert!(turn_a.is_cancelled());
        assert!(!session.is_cancelled());
        assert!(!turn_b.is_cancelled());
        assert_eq!(turn_b.run(async { 7 }).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn already_cancelled_token_does_not_start_work() {
        let token = CancelToken::new();
        token.cancel();
        let mut started = false;
        let outcome = token.run(async { started = true }).await;
        assert!(outcome.is_err());
        assert!(!started);
    }
}
