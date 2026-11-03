//! The admission steps of group chats and roles (spec: messaging-gateway,
//! Authorization order and fail-closed default; Role-based access grants;
//! Bot-authored message admission; Group mention gating and observe mode;
//! Channel history backfill). The decisions are `gating`'s; this module
//! applies them to an inbound event with the profile's configuration and
//! adapter.

use std::sync::Arc;

use pmt_config::schema::GatewayConfig;
use pmt_store::Store;
use tokio::time::Instant;

use super::Shared;
use super::adapter::{Admission, Descriptor, Inbound, SenderVerdict};
use super::auth::{self, Facts, Rule};
use super::gating::{self, Gate};

/// The authorization order: the gateway's own rules, then an
/// `allow_roles` grant (right after `allow_users`), then the adapter's
/// allowlist; anything else is denied.
pub(crate) async fn authorized(
    shared: &Shared,
    owner: &str,
    platform: &str,
    store: &Store,
    descriptor: &Descriptor,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> bool {
    let table = gateway.platform(platform);
    let paired = store
        .is_paired(platform, &inbound.sender_id)
        .unwrap_or_else(|err| {
            tracing::warn!("pairing lookup failed: {err}");
            false
        });
    let mut facts = Facts {
        descriptor,
        config: &table,
        inbound,
        paired,
        adapter: SenderVerdict::Unknown,
    };
    if auth::authorize(&facts).is_some() {
        return true;
    }
    let adapter = shared.adapters.adapter(owner, platform);
    if let Some(adapter) = &adapter
        && descriptor.exposes_roles
        && !table.allow_roles.is_empty()
    {
        let key = format!(
            "{owner}\u{1f}{platform}\u{1f}{}\u{1f}{}",
            inbound.scope_id.as_deref().unwrap_or(&inbound.chat_id),
            inbound.sender_id
        );
        if shared
            .roles
            .holds(&key, adapter.as_ref(), inbound, &table.allow_roles)
            .await
        {
            tracing::debug!(profile = owner, platform, rule = ?Rule::Role, "admitted");
            return true;
        }
    }
    if auth::asks_adapter(descriptor)
        && let Some(adapter) = adapter
    {
        facts.adapter = auth::adapter_verdict(adapter.as_ref(), inbound).await;
        return auth::authorize(&facts).is_some();
    }
    false
}

/// Whether a message is admitted as far as its author being a bot goes:
/// people always are; a bot per `gateway.bots` (its burst follow-ups
/// included).
pub(crate) fn bot_admitted(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> bool {
    if !inbound.is_bot {
        return true;
    }
    let bots = gateway.bots.for_platform(platform);
    if bots.human_user_ids.contains(&inbound.sender_id) {
        return true;
    }
    let key = format!(
        "{owner}\u{1f}{platform}\u{1f}{}\u{1f}{}",
        inbound.chat_id, inbound.sender_id
    );
    let admitted = shared.bursts.admit(&bots, &key, inbound, Instant::now());
    if !admitted {
        tracing::debug!(profile = owner, platform, "bot message not admitted");
    }
    admitted
}

/// Applies the group gate: the event as it starts a turn (with the
/// channel backfill in front when it was mention-triggered), or the
/// admission of one that does not.
pub(crate) async fn gate(
    shared: &Arc<Shared>,
    owner: &str,
    platform: &str,
    mut inbound: Inbound,
    gateway: &GatewayConfig,
) -> Result<Inbound, Admission> {
    let groups = gateway.groups.for_platform(platform);
    match gating::group_gate(&groups, &inbound) {
        Gate::Ignore => Err(Admission::Dropped),
        Gate::Observe => {
            let chat = gating::chat_key(
                owner,
                platform,
                &inbound.chat_id,
                inbound.thread_id.as_deref(),
            );
            shared.observed.push(&chat, gating::observed_line(&inbound));
            Err(Admission::Handled)
        }
        Gate::Turn { gated } => {
            if gated && groups.history_backfill {
                backfill(
                    shared,
                    owner,
                    platform,
                    &mut inbound,
                    groups.history_backfill_limit,
                )
                .await;
            }
            Ok(inbound)
        }
    }
}

/// Puts the channel's messages since the bot's last reply in front of a
/// mention-triggered message (adapters that can fetch history).
async fn backfill(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &mut Inbound,
    limit: usize,
) {
    let can = shared
        .adapters
        .descriptor(owner, platform)
        .is_some_and(|d| d.can_fetch_history);
    let Some(adapter) = shared.adapters.adapter(owner, platform).filter(|_| can) else {
        return;
    };
    // One more than the limit: the triggering message may be among them.
    let fetched = adapter
        .fetch_history(&inbound.chat_id, inbound.thread_id.as_deref(), limit + 1)
        .await;
    match fetched {
        Ok(history) => {
            let lines = gating::since_last_reply(&history, &inbound.message_id, limit);
            inbound.text = gating::with_backfill(&lines, &inbound.text);
        }
        Err(err) => tracing::warn!(
            profile = owner,
            platform,
            "channel history not fetched: {}",
            err.describe()
        ),
    }
}
