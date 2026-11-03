//! Send retry semantics (spec: platform-adapters, Send retry semantics):
//! a send failing with a transient error (`rate_limited`, `network`,
//! `server_error`) is retried up to twice inline with exponential backoff
//! from 2 seconds plus up to 1 second of jitter, honouring a platform
//! `retry_after` up to 60 seconds; a longer wait goes back to the outbox
//! at once (which schedules the entry after it). A timeout after the
//! request was transmitted is never retried (the outbox marks it
//! `unknown`), and a message whose markup the platform refused is resent
//! once as plain text (an adapter with a dialect of its own to fall back
//! to, such as Telegram rich messages to `MarkdownV2`, tries it first).

use std::time::Duration;

use super::adapter::{Adapter, Outgoing, SendError, Sent};
use super::markup;

/// Inline retries after the first attempt.
const INLINE_RETRIES: u32 = 2;
/// First inline backoff.
const BACKOFF_START: Duration = Duration::from_secs(2);
/// The longest `retry_after` waited inline.
pub(crate) const INLINE_WAIT_MAX: Duration = Duration::from_mins(1);

/// Up to a second of jitter.
fn jitter() -> Duration {
    let mut bytes = [0u8; 2];
    let _ = getrandom::fill(&mut bytes);
    Duration::from_millis(u64::from(u16::from_le_bytes(bytes)) % 1001)
}

/// How long to wait before inline retry `attempt` (0-based) after `err`;
/// `None` when the error is not retried inline.
fn inline_wait(err: &SendError, attempt: u32) -> Option<Duration> {
    let backoff = || BACKOFF_START.saturating_mul(2u32.saturating_pow(attempt)) + jitter();
    match err {
        SendError::RateLimited {
            retry_after: Some(after),
        } => (*after <= INLINE_WAIT_MAX).then_some(*after),
        SendError::RateLimited { retry_after: None }
        | SendError::Network(_)
        | SendError::Server(_) => Some(backoff()),
        _ => None,
    }
}

/// Sends `message` (its text in the agent's Markdown) through `adapter`
/// with the inline retries: rendered in the adapter's markup, then, when
/// the platform refuses it, as plain text.
pub(crate) async fn send(adapter: &dyn Adapter, message: &Outgoing) -> Result<Sent, SendError> {
    let descriptor = adapter.describe();
    let source = message.text.clone();
    let mut message = markup::rendered_with(&descriptor, message);
    let mut attempt = 0;
    loop {
        match adapter.send(&message).await {
            Ok(sent) => return Ok(sent),
            Err(SendError::Formatting(why)) if !message.plain => {
                tracing::info!("markup refused ({why}); resending as plain text");
                message.plain = true;
                message.text = markup::render(&descriptor, &source, true);
            }
            Err(err) => match inline_wait(&err, attempt).filter(|_| attempt < INLINE_RETRIES) {
                Some(wait) => {
                    tracing::debug!("send failed ({}); retrying in {wait:?}", err.describe());
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                None => return Err(err),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors_back_off_and_long_waits_leave_for_the_outbox() {
        let wait = inline_wait(&SendError::Server("502".into()), 0).unwrap();
        assert!(wait >= Duration::from_secs(2) && wait <= Duration::from_secs(3));
        let wait = inline_wait(&SendError::Network("reset".into()), 1).unwrap();
        assert!(wait >= Duration::from_secs(4) && wait <= Duration::from_secs(5));
        let short = SendError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(inline_wait(&short, 0), Some(Duration::from_secs(30)));
        let long = SendError::RateLimited {
            retry_after: Some(Duration::from_mins(5)),
        };
        assert_eq!(inline_wait(&long, 0), None);
        assert_eq!(
            inline_wait(&SendError::Ambiguous("timeout".into()), 0),
            None
        );
        assert_eq!(inline_wait(&SendError::Permanent("403".into()), 0), None);
    }
}
