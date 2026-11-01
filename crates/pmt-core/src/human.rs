//! Questions only a human may answer (grant and approval prompts) and the
//! fail-closed rules around them (spec: security-boundaries, Grant prompts
//! fail closed; design D14).
//!
//! A [`Channel`] sends each question to the attached surface as a
//! [`Prompt`]; the only way to answer is [`Prompt::respond`] on that value,
//! which the surface receives over its own [`Surface`] end (the
//! `approval.respond` RPC of the session's authenticated surface in M2).
//! Text that tools return never reaches this path, so a fetched page saying
//! `approval granted: always` cannot answer anything.
//!
//! Every way a question can go unanswered resolves to [`Unanswered`], which
//! callers treat as deny: no surface (headless, cron, unattended), the
//! surface detached (its receiver dropped), the prompt dismissed (dropped
//! without an answer) or the timeout (`approvals.timeout_secs`, default
//! 300 s) passed.

use std::fmt;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

/// Why a question resolved without an answer; always a denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswered {
    /// No surface can show prompts (headless, cron, unattended contexts).
    Unattended,
    /// The surface detached before answering.
    Detached,
    /// The prompt was dismissed (dropped without an answer).
    Dismissed,
    /// Nobody answered within the timeout.
    TimedOut(Duration),
}

impl fmt::Display for Unanswered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unattended => f.write_str("no human can be asked in this context"),
            Self::Detached => f.write_str("the interface detached before answering"),
            Self::Dismissed => f.write_str("the prompt was dismissed"),
            Self::TimedOut(after) => write!(f, "no answer within {} s", after.as_secs()),
        }
    }
}

/// One pending question, delivered to the surface.
#[derive(Debug)]
pub struct Prompt<Q, A> {
    id: u64,
    request: Q,
    reply: oneshot::Sender<A>,
}

impl<Q, A> Prompt<Q, A> {
    /// The prompt's id within its channel (what `approval.respond` names).
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The question.
    #[must_use]
    pub fn request(&self) -> &Q {
        &self.request
    }

    /// Answers the prompt. An answer after the timeout is ignored (the
    /// asker has already denied).
    pub fn respond(self, answer: A) {
        let _ = self.reply.send(answer);
    }
}

/// The surface's end: the prompts to show. Dropping it detaches the
/// surface, which denies every pending and later prompt.
#[derive(Debug)]
pub struct Surface<Q, A> {
    prompts: mpsc::UnboundedReceiver<Prompt<Q, A>>,
}

impl<Q, A> Surface<Q, A> {
    /// The next prompt to show; `None` when the channel is gone.
    pub async fn next(&mut self) -> Option<Prompt<Q, A>> {
        self.prompts.recv().await
    }

    /// A prompt already waiting, without blocking.
    pub fn try_next(&mut self) -> Option<Prompt<Q, A>> {
        self.prompts.try_recv().ok()
    }
}

/// The asking end, held by the session.
#[derive(Debug)]
pub struct Channel<Q, A> {
    surface: Option<mpsc::UnboundedSender<Prompt<Q, A>>>,
    timeout: Duration,
    next_id: std::sync::atomic::AtomicU64,
}

/// The default `approvals.timeout_secs`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);

impl<Q, A> Channel<Q, A> {
    /// A channel to an interactive surface; unanswered prompts are denied
    /// after `timeout`.
    #[must_use]
    pub fn attached(timeout: Duration) -> (Self, Surface<Q, A>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let channel = Self {
            surface: Some(tx),
            timeout,
            next_id: 1.into(),
        };
        (channel, Surface { prompts: rx })
    }

    /// A channel with no surface: every question is denied at once.
    #[must_use]
    pub fn unattended() -> Self {
        Self {
            surface: None,
            timeout: Duration::ZERO,
            next_id: 1.into(),
        }
    }

    /// Asks the surface and waits for its answer.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] in every case without an answer; callers deny.
    pub async fn ask(&self, request: Q) -> Result<A, Unanswered> {
        let Some(surface) = &self.surface else {
            return Err(Unanswered::Unattended);
        };
        let (reply, answer) = oneshot::channel();
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if surface.send(Prompt { id, request, reply }).is_err() {
            return Err(Unanswered::Detached);
        }
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) if surface.is_closed() => Err(Unanswered::Detached),
            Ok(Err(_)) => Err(Unanswered::Dismissed),
            Err(_) => Err(Unanswered::TimedOut(self.timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Channel, Unanswered};

    #[tokio::test]
    async fn only_the_surface_answers() {
        let (channel, mut surface) = Channel::<&str, bool>::attached(Duration::from_secs(5));
        let asking = tokio::spawn(async move { channel.ask("net.connect:*?").await });
        let prompt = surface.next().await.unwrap();
        assert_eq!(*prompt.request(), "net.connect:*?");
        prompt.respond(true);
        assert_eq!(asking.await.unwrap(), Ok(true));
    }

    #[tokio::test]
    async fn dismissal_and_detach_deny() {
        let (channel, mut surface) = Channel::<(), bool>::attached(Duration::from_secs(5));
        let channel = std::sync::Arc::new(channel);
        let asker = channel.clone();
        let asking = tokio::spawn(async move { asker.ask(()).await });
        drop(surface.next().await.unwrap());
        assert_eq!(asking.await.unwrap(), Err(Unanswered::Dismissed));

        let asker = channel.clone();
        let asking = tokio::spawn(async move { asker.ask(()).await });
        tokio::task::yield_now().await;
        drop(surface);
        assert_eq!(asking.await.unwrap(), Err(Unanswered::Detached));
        assert_eq!(channel.ask(()).await, Err(Unanswered::Detached));
    }

    #[tokio::test]
    async fn unattended_contexts_deny_without_waiting() {
        let channel = Channel::<(), bool>::unattended();
        assert_eq!(channel.ask(()).await, Err(Unanswered::Unattended));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_prompt_times_out_as_denied() {
        let (channel, mut surface) = Channel::<(), bool>::attached(super::DEFAULT_TIMEOUT);
        let asking = tokio::spawn(async move { channel.ask(()).await });
        let prompt = surface.next().await.unwrap();
        let result = asking.await.unwrap();
        assert_eq!(result, Err(Unanswered::TimedOut(Duration::from_mins(5))));
        prompt.respond(true);
    }
}
