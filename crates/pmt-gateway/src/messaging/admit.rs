//! An inbound event's way in (spec: messaging-gateway, Routing identity
//! and deterministic session keys; Authorization order and fail-closed
//! default; Documented pairing flow): the owning profile must be served,
//! the sender authorized (or offered pairing in a DM), a bot message must
//! pass its chat's loop guard, and then the event joins the lane of its
//! conversation key.

use std::sync::Arc;
use std::time::Duration;

use jiff::{Timestamp, Zoned};
use pmt_config::schema::{ChatPolicy, GatewayConfig, PolicyFacts, UnauthorizedDm, resolve_policy};
use pmt_store::gateway::pairing::{Requested, Requester};
use serde_json::json;
use tokio::time::Instant;

use super::Shared;
use super::adapter::{Admission, Inbound, Outgoing};
use super::auth::{self, Facts};
use super::inbound;
use super::key::{ChatType, Isolation, Routing};
use super::lane;
use super::limits::{self, Verdict};

/// How often an adapter's events for an unserved profile are logged.
const UNSERVED_WARN_EVERY: Duration = Duration::from_hours(1);

/// Admits one event that adapter `platform` of `profile` received.
pub(crate) async fn admit(
    shared: &Arc<Shared>,
    profile: &str,
    platform: &str,
    inbound: Inbound,
) -> Admission {
    if shared.stop.is_cancelled() {
        return Admission::Dropped;
    }
    let owner = inbound
        .profile
        .clone()
        .unwrap_or_else(|| profile.to_owned());
    let Some(scope) = shared.agents.scope(&owner) else {
        warn_unserved(shared, profile, platform, &owner);
        return Admission::Dropped;
    };
    let hooks = &shared.options.hooks;
    let event = json!({"platform": platform, "chat_type": inbound.chat_type.as_str()});
    hooks
        .emit(&owner, "pre_gateway_dispatch", event.clone())
        .await;
    let gateway = &scope.config().gateway;
    let admission = decide(shared, &owner, platform, inbound, gateway).await;
    let label = json!(admission);
    shared.health.add(
        "prometheus.gateway.admissions",
        &[
            ("platform", platform),
            ("admission", label.as_str().unwrap_or_default()),
        ],
    );
    let mut event = event;
    event["admission"] = label;
    hooks.emit(&owner, "post_gateway_admission", event).await;
    admission
}

async fn decide(
    shared: &Arc<Shared>,
    owner: &str,
    platform: &str,
    inbound: Inbound,
    gateway: &GatewayConfig,
) -> Admission {
    let Some(descriptor) = shared.adapters.descriptor(owner, platform) else {
        return Admission::Dropped;
    };
    let Some(store) = shared.store(owner) else {
        return Admission::Dropped;
    };
    let table = gateway.platform(platform);
    let paired = store
        .is_paired(platform, &inbound.sender_id)
        .unwrap_or_else(|err| {
            tracing::warn!("pairing lookup failed: {err}");
            false
        });
    let facts = Facts {
        descriptor: &descriptor,
        config: &table,
        inbound: &inbound,
        paired,
    };
    if auth::authorize(&facts).is_none() {
        return unauthorized(shared, owner, platform, &inbound, gateway).await;
    }
    shared
        .last_inbound
        .record(owner, platform, &inbound.chat_id, &inbound.message_id);
    let chat = format!("{owner}:{platform}:{}", inbound.chat_id);
    if inbound.is_bot && !shared.guard.admit(&chat, Instant::now()) {
        tracing::info!(
            profile = owner,
            platform,
            "bot message refused: bot-loop cool-down"
        );
        return Admission::Dropped;
    }
    let dedup = &gateway.dedup;
    let duplicate = shared.dedup.seen(
        &format!("{owner}/{platform}"),
        &inbound.chat_id,
        &inbound.message_id,
        Duration::from_secs(dedup.ttl_secs),
        dedup.max_entries as usize,
        Instant::now(),
    );
    if duplicate {
        tracing::debug!(profile = owner, platform, "dropped a redelivered event");
        return Admission::Duplicate;
    }
    if let Some(limited) = limit(shared, owner, platform, &inbound, gateway).await {
        return limited;
    }
    let mut inbound = inbound;
    let voice = match shared.agents.scope(owner) {
        Some(scope) => inbound::prepare(shared, owner, platform, &scope, &mut inbound).await,
        None => false,
    };
    let (group_per_user, thread_per_user) = gateway.sessions_for(platform);
    let routing = Routing {
        adapter: format!("{owner}/{platform}"),
        profile: owner.to_owned(),
        platform: platform.to_owned(),
        scope_id: inbound.scope_id.clone(),
        chat_id: inbound.chat_id.clone(),
        chat_type: inbound.chat_type,
        thread_id: inbound.thread_id.clone(),
        sender_id: inbound.sender_id.clone(),
        person_id: None,
    };
    let key = routing.key(Isolation {
        group_per_user,
        thread_per_user,
    });
    lane::input(shared, &key, routing, inbound, voice)
}

/// The policy matching an event, if any.
pub(crate) fn policy_for<'a>(
    gateway: &'a GatewayConfig,
    platform: &str,
    inbound: &Inbound,
) -> Option<(usize, &'a ChatPolicy)> {
    let facts = PolicyFacts {
        platform,
        chat_id: &inbound.chat_id,
        chat_type: inbound.chat_type.as_str(),
        user_id: &inbound.sender_id,
    };
    resolve_policy(&gateway.chat_policies, &facts)
}

/// Applies the sender's rate limits and daily budget (admins are exempt);
/// `Some(rate_limited)` drops the event.
async fn limit(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> Option<Admission> {
    let table = gateway.platform(platform);
    if auth::is_admin(&table, inbound.chat_type, &inbound.sender_id) == Some(true) {
        return None;
    }
    let mut config = gateway.rate_limit_for(platform);
    if let Some(over) =
        policy_for(gateway, platform, inbound).and_then(|(_, p)| p.rate_limit.as_ref())
    {
        config = config.with(over);
    }
    let turn = inbound.callback.is_none() && !inbound.text.trim_start().starts_with('/');
    let sender = format!("{owner}\u{1f}{platform}\u{1f}{}", inbound.sender_id);
    let now = Instant::now();
    if let Verdict::Limited { wait, notify } = shared.limits.check(&sender, &config, turn, now) {
        tracing::info!(profile = owner, platform, "message dropped: rate limit");
        if notify {
            reply(shared, owner, platform, inbound, &limits::slow_down(wait)).await;
        }
        return Some(Admission::RateLimited);
    }
    let cap = config.daily_cost_usd.filter(|_| turn)?;
    let store = shared.store(owner)?;
    let midnight = Zoned::now()
        .start_of_day()
        .map_or_else(|_| Timestamp::now(), |z| z.timestamp());
    let spent = store
        .sender_cost_since(platform, &inbound.sender_id, midnight)
        .unwrap_or_else(|err| {
            tracing::warn!("daily spend not readable: {err}");
            0.0
        });
    if spent < cap {
        return None;
    }
    tracing::info!(
        profile = owner,
        platform,
        "message dropped: daily budget spent"
    );
    if shared.limits.budget_notice_due(&sender, now) {
        reply(
            shared,
            owner,
            platform,
            inbound,
            &limits::daily_budget(spent, cap),
        )
        .await;
    }
    Some(Admission::RateLimited)
}

/// An unauthorized sender: silence in groups; in a DM the platform's
/// `unauthorized_dm` policy (a pairing code, a refusal or nothing).
async fn unauthorized(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> Admission {
    if inbound.chat_type != ChatType::Dm {
        return Admission::Unauthorized;
    }
    match gateway.unauthorized_dm(platform) {
        UnauthorizedDm::Ignore => Admission::Unauthorized,
        UnauthorizedDm::Decline => {
            reply(shared, owner, platform, inbound, DECLINED).await;
            Admission::Unauthorized
        }
        UnauthorizedDm::Pair => pair(shared, owner, platform, inbound).await,
    }
}

const DECLINED: &str = "You are not authorized to use this assistant.";

async fn pair(shared: &Shared, owner: &str, platform: &str, inbound: &Inbound) -> Admission {
    let Some(store) = shared.store(owner) else {
        return Admission::Unauthorized;
    };
    let who = Requester {
        platform,
        sender_id: &inbound.sender_id,
        chat_id: &inbound.chat_id,
        sender_name: inbound.sender_name.as_deref(),
    };
    match store.request_pairing(&who, Timestamp::now()) {
        Ok(Requested::Issued(code)) => {
            // Sent directly, never through the outbox: the code is stored
            // nowhere in plain text.
            let text = pairing_text(owner, platform, &code);
            reply(shared, owner, platform, inbound, &text).await;
            tracing::info!(profile = owner, platform, "pairing code issued");
            Admission::PairingIssued
        }
        Ok(other) => {
            tracing::debug!(
                profile = owner,
                platform,
                "no pairing code issued: {other:?}"
            );
            Admission::Unauthorized
        }
        Err(err) => {
            tracing::warn!(profile = owner, platform, "pairing request failed: {err}");
            Admission::Unauthorized
        }
    }
}

fn pairing_text(owner: &str, platform: &str, code: &str) -> String {
    let profile = if owner == "default" {
        String::new()
    } else {
        format!("-p {owner} ")
    };
    format!(
        "This assistant does not know you yet. Your pairing code is {code} (valid for one hour). \
         Ask its owner to approve it with: prometheus {profile}pairing approve {platform} {code}"
    )
}

pub(crate) async fn reply(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    text: &str,
) {
    let message = Outgoing::text(&inbound.chat_id, inbound.thread_id.as_deref(), text);
    if let Err(err) = shared.adapters.send_direct(owner, platform, &message).await {
        tracing::warn!(
            profile = owner,
            platform,
            "reply not sent: {}",
            err.describe()
        );
    }
}

/// One WARNING per adapter per hour for events addressed to a profile the
/// daemon does not serve (never routed to another profile).
fn warn_unserved(shared: &Shared, profile: &str, platform: &str, owner: &str) {
    let adapter = format!("{profile}/{platform}");
    let now = Instant::now();
    let mut warned = shared.unserved.lock().expect("unserved poisoned");
    let due = warned
        .get(&adapter)
        .is_none_or(|at| now.duration_since(*at) >= UNSERVED_WARN_EVERY);
    if due {
        warned.insert(adapter.clone(), now);
        tracing::warn!(
            adapter,
            "dropped an event for profile '{owner}', which this daemon does not serve"
        );
    }
}
