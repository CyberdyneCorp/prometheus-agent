//! Delivery and read receipts, and failed deliveries, surfaced (spec:
//! messaging-gateway, Delivery receipts surfaced; platform-adapters,
//! Delivery and read receipts): an adapter declaring `delivery_receipts`
//! or `read_receipts` reports the platform id it returned from `send`;
//! the outbox entry moves to `delivered` or `read` and the session's
//! attached clients get `message.delivery {session_id, outbox_id,
//! state}`. A receipt for an unknown id is ignored. An entry that ends
//! `failed` is announced the same way and, when the platform has a home
//! channel, told there too.

use pmt_store::gateway::outbox::{NewOutbound, Outbound, OutboundKind, OutboundState};
use serde_json::json;

use super::Shared;
use super::adapter::Receipt;

/// Applies `receipt` from `platform` of `profile`.
pub(crate) fn receipt(shared: &Shared, profile: &str, platform: &str, receipt: &Receipt) {
    let Some(store) = shared.store(profile) else {
        return;
    };
    let state = if receipt.read {
        OutboundState::Read
    } else {
        OutboundState::Delivered
    };
    match store.outbound_receipt(platform, &receipt.message_id, state) {
        Ok(Some(entry)) => announce(shared, &entry),
        Ok(None) => tracing::debug!(platform, "receipt for an unknown or settled message"),
        Err(err) => tracing::warn!("receipt not recorded: {err}"),
    }
}

/// Tells the session's attached clients the entry's state.
pub(crate) fn announce(shared: &Shared, entry: &Outbound) {
    let Some(session) = &entry.session_id else {
        return;
    };
    shared.server.hub().notify_session(
        session,
        "message.delivery",
        json!({
            "session_id": session,
            "outbox_id": entry.id,
            "state": entry.state.as_str(),
        }),
    );
}

/// An entry gave up: its session's clients are told, and the platform's
/// home channel (unless the failure was there).
pub(crate) fn failed(shared: &Shared, profile: &str, id: i64) {
    let Some(entry) = shared
        .store(profile)
        .and_then(|store| store.outbound(id).ok().flatten())
        .filter(|e| e.state == OutboundState::Failed)
    else {
        return;
    };
    announce(shared, &entry);
    let home = shared
        .platform_config(profile, &entry.platform)
        .home_channel
        .filter(|home| home.chat_id != entry.chat_id);
    let Some(home) = home else {
        return;
    };
    let strings = shared.strings(profile);
    let excerpt: String = entry.body.chars().take(120).collect();
    let text = pmt_i18n::t!(
        strings,
        "gateway.delivery_failed",
        chat = entry.chat_id,
        excerpt = excerpt
    );
    shared.deliver(
        profile,
        &NewOutbound {
            idempotency_key: format!("delivery-failed:{}", entry.id),
            session_id: None,
            platform: entry.platform.clone(),
            chat_id: home.chat_id,
            thread_id: home.thread_id,
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::Warning,
            body: text,
            attachments: Vec::new(),
            notify: None,
        },
    );
}
