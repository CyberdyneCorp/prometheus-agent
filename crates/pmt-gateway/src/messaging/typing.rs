//! Typing indicator and human delay (spec: platform-adapters, Typing
//! indicator and human delay): while a turn runs the platform's typing
//! indicator is refreshed every 2 seconds, each call bounded to 1.5
//! seconds so a slow one never stalls the cadence (failures are
//! ignored); before each outbound part a uniformly random delay within
//! the platform's `human_delay_ms = [min, max]` is inserted.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::{CancellationToken, DropGuard};

use super::Shared;
use super::adapter::SendError;
use super::adapters::Op;

/// The refresh cadence.
pub(crate) const EVERY: Duration = Duration::from_secs(2);
/// The bound of one indicator call (within 0.25 to 1.5 seconds, below
/// the cadence so calls never overlap).
pub(crate) const CALL_BOUND: Duration = Duration::from_millis(1500);

/// Shows the typing indicator in a chat until the guard drops; `None`
/// when the platform has none or `typing_indicator = false`.
pub(crate) fn start(
    shared: &Arc<Shared>,
    profile: &str,
    platform: &str,
    chat_id: &str,
    thread_id: Option<&str>,
) -> Option<DropGuard> {
    let enabled = shared
        .agents
        .scope(profile)
        .is_none_or(|s| s.config().gateway.platform(platform).typing());
    let slot = shared.adapters.ready(profile, platform)?;
    if !enabled || !slot.can(Op::Typing) {
        return None;
    }
    let stop = CancellationToken::new();
    let (chat, thread) = (chat_id.to_owned(), thread_id.map(str::to_owned));
    let token = stop.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(EVERY);
        loop {
            tokio::select! {
                () = token.cancelled() => return,
                _ = ticker.tick() => {}
            }
            if !slot.can(Op::Typing) {
                return;
            }
            let call = slot.adapter.send_typing(&chat, thread.as_deref());
            match tokio::time::timeout(CALL_BOUND, call).await {
                Ok(Err(SendError::Unsupported)) => slot.refused(Op::Typing),
                Ok(Err(err)) => tracing::debug!("typing indicator failed: {}", err.describe()),
                Ok(Ok(())) | Err(_) => {}
            }
        }
    });
    Some(stop.drop_guard())
}

/// The delay before one outbound part on `platform`.
pub(crate) fn human_delay(range: Option<[u64; 2]>) -> Duration {
    let Some([min, max]) = range.filter(|[min, max]| max >= min) else {
        return Duration::ZERO;
    };
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let span = max - min + 1;
    Duration::from_millis(min + u64::from_le_bytes(bytes) % span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_delay_stays_within_its_range() {
        assert_eq!(human_delay(None), Duration::ZERO);
        for _ in 0..50 {
            let delay = human_delay(Some([100, 200]));
            assert!(delay >= Duration::from_millis(100) && delay <= Duration::from_millis(200));
        }
        assert_eq!(human_delay(Some([5, 5])), Duration::from_millis(5));
    }
}
