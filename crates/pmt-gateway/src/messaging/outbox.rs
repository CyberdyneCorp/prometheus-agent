//! The delivery worker of one profile (spec: messaging-gateway, Durable
//! delivery outbox): due entries are sent in order per chat; a throttled
//! send is retried with exponential backoff from 2 seconds up to 10
//! minutes and counts against `gateway.outbox.max_attempts`; while the
//! platform is unreachable or failing (network, 5xx) entries wait without
//! using attempts, bounded by `gateway.outbox.max_age_secs`; an attempt
//! whose outcome is unknown is never resent automatically.

use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_store::Store;
use pmt_store::gateway::outbox::{Outbound, OutboundKind, OutboundState};
use serde_json::{Value, json};
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{Button, Outgoing, Select, SendError};
use super::adapters::Op;
use super::{markup, retry, typing};

/// How often the worker looks for due entries without being woken (the
/// CLI enqueues from another process).
const POLL: Duration = Duration::from_secs(1);
/// First retry delay.
const BACKOFF_START: Duration = Duration::from_secs(2);
/// Longest retry delay.
const BACKOFF_MAX: Duration = Duration::from_mins(10);
/// Entries handled per round.
const BATCH: usize = 32;

/// Starts `profile`'s delivery worker.
pub(crate) fn spawn(shared: &Arc<Shared>, profile: &str) {
    let span = tracing::info_span!("outbox", profile);
    tokio::spawn(run(Arc::clone(shared), profile.to_owned()).instrument(span));
}

async fn run(shared: Arc<Shared>, profile: String) {
    loop {
        if let Some(store) = shared.store(&profile) {
            let due = store
                .due_outbound(Timestamp::now(), BATCH)
                .unwrap_or_else(|err| {
                    tracing::warn!("outbox not readable: {err}");
                    Vec::new()
                });
            for entry in due {
                deliver(&shared, &profile, &store, entry).await;
            }
        }
        let wake = shared.outbox_wake.notified();
        tokio::select! {
            () = wake => {}
            () = tokio::time::sleep(POLL) => {}
        }
    }
}

/// The delay before attempt `attempts + 1` of a throttled entry.
pub(crate) fn backoff(attempts: u32) -> Duration {
    BACKOFF_START
        .saturating_mul(2u32.saturating_pow(attempts))
        .min(BACKOFF_MAX)
}

/// The delay of an entry waiting out an outage: as long as it has waited
/// so far (so the waits double), within the backoff bounds.
fn outage_wait(entry: &Outbound, now: Timestamp) -> Duration {
    let age = now.duration_since(entry.created_at);
    Duration::try_from(age)
        .unwrap_or_default()
        .clamp(BACKOFF_START, BACKOFF_MAX)
}

fn after(now: Timestamp, wait: Duration) -> Timestamp {
    now.saturating_add(SignedDuration::try_from(wait).unwrap_or(SignedDuration::MAX))
        .unwrap_or(now)
}

/// The message an entry sends: a card's JSON carries buttons.
fn outgoing(entry: &Outbound) -> Outgoing {
    let (text, buttons, select) = if entry.kind == OutboundKind::Approval {
        let card: Value = serde_json::from_str(&entry.body).unwrap_or_else(|_| json!({}));
        let buttons: Vec<Button> =
            serde_json::from_value(card["buttons"].clone()).unwrap_or_default();
        let select: Option<Select> = serde_json::from_value(card["select"].clone()).ok();
        (
            card["text"].as_str().unwrap_or_default().to_owned(),
            buttons,
            select,
        )
    } else {
        (entry.body.clone(), Vec::new(), None)
    };
    Outgoing {
        chat_id: entry.chat_id.clone(),
        thread_id: entry.thread_id.clone(),
        reply_to: entry.reply_to.clone(),
        text,
        buttons,
        select,
        attachments: entry.attachments.clone(),
        plain: false,
    }
}

async fn deliver(shared: &Shared, profile: &str, store: &Store, entry: Outbound) {
    let config = shared.outbox_config(profile);
    let now = Timestamp::now();
    let max_age = SignedDuration::from_secs(i64::try_from(config.max_age_secs).unwrap_or(i64::MAX));
    if now.duration_since(entry.created_at) > max_age {
        settle(
            store,
            &entry,
            OutboundState::Failed,
            "expired before it could be sent",
        );
        return;
    }
    let Some(slot) = shared.adapters.ready(profile, &entry.platform) else {
        // No connected adapter: the entry waits, using no attempt.
        return;
    };
    match store.start_sending(entry.id) {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            tracing::warn!("outbox entry {} not claimed: {err}", entry.id);
            return;
        }
    }
    let message = outgoing(&entry);
    let delay = typing::human_delay(
        shared
            .platform_config(profile, &entry.platform)
            .human_delay_ms,
    );
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let outcome = match &entry.edit_of {
        Some(id) if slot.can(Op::Edit) => match slot
            .adapter
            .edit(id, &markup::rendered(slot.adapter.as_ref(), &message))
            .await
        {
            Ok(()) => Ok(Some(id.clone())),
            Err(err @ (SendError::Unsupported | SendError::Permanent(_))) => {
                if err == SendError::Unsupported {
                    slot.refused(Op::Edit);
                }
                retry::send(slot.adapter.as_ref(), &message)
                    .await
                    .map(|s| s.message_id)
            }
            Err(err) => Err(err),
        },
        _ => retry::send(slot.adapter.as_ref(), &message)
            .await
            .map(|s| s.message_id),
    };
    match outcome {
        Ok(message_id) => {
            if let Err(err) = store.sent(entry.id, message_id.as_deref()) {
                tracing::warn!("outbox entry {} sent but not recorded: {err}", entry.id);
            }
            shared.adapters.confirm(profile, &entry.platform);
            shared
                .health
                .add("prometheus.outbox.sent", &[("platform", &entry.platform)]);
            shared
                .options
                .hooks
                .emit(
                    profile,
                    "post_gateway_delivery",
                    json!({"platform": entry.platform, "kind": entry.kind.as_str()}),
                )
                .await;
        }
        Err(err) => {
            shared.health.add(
                "prometheus.outbox.send_failures",
                &[("platform", &entry.platform)],
            );
            failed(store, &entry, &err, config.max_attempts, now);
        }
    }
}

fn failed(store: &Store, entry: &Outbound, err: &SendError, max_attempts: u32, now: Timestamp) {
    let reason = err.describe();
    let result = match err {
        SendError::RateLimited { retry_after } => {
            if entry.attempts + 1 >= max_attempts {
                store.settle_outbound(entry.id, OutboundState::Failed, &reason)
            } else {
                let wait = backoff(entry.attempts).max(retry_after.unwrap_or_default());
                store.retry_later(entry.id, after(now, wait), true, &reason)
            }
        }
        SendError::Network(_) | SendError::Server(_) => store.retry_later(
            entry.id,
            after(now, outage_wait(entry, now)),
            false,
            &reason,
        ),
        SendError::Ambiguous(_) => store.settle_outbound(entry.id, OutboundState::Unknown, &reason),
        SendError::Permanent(_) | SendError::Formatting(_) | SendError::Unsupported => {
            store.settle_outbound(entry.id, OutboundState::Failed, &reason)
        }
    };
    if let Err(store_err) = result {
        tracing::warn!("outbox entry {} not updated: {store_err}", entry.id);
    }
    tracing::info!(entry = entry.id, "send failed: {reason}");
}

fn settle(store: &Store, entry: &Outbound, state: OutboundState, reason: &str) {
    if let Err(err) = store.settle_outbound(entry.id, state, reason) {
        tracing::warn!("outbox entry {} not updated: {err}", entry.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttled_retries_back_off_from_two_seconds_to_ten_minutes() {
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(5), Duration::from_secs(64));
        assert_eq!(backoff(20), Duration::from_mins(10));
    }
}
