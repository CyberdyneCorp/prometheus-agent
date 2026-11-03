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
use super::adapter::{Admission, Descriptor, Inbound, Outgoing, SenderVerdict};
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

/// The command a message stands for on a platform with a command prefix
/// alias: its first token (after an optional leading bot mention) is
/// `<alias><command>`; only that token is looked at.
pub(crate) fn command_alias(descriptor: &Descriptor, text: &str) -> Option<String> {
    let alias = descriptor.command_prefix_alias.as_deref()?;
    let mut rest = text.trim_start();
    let first = rest.split_whitespace().next()?;
    let mention = (first.starts_with("<@") && first.ends_with('>')) || first.starts_with('@');
    if mention {
        rest = rest[first.len()..].trim_start();
    }
    let command = rest.strip_prefix(alias)?;
    command
        .chars()
        .next()
        .is_some_and(char::is_alphanumeric)
        .then(|| format!("/{command}"))
}

async fn decide(
    shared: &Arc<Shared>,
    owner: &str,
    platform: &str,
    mut inbound: Inbound,
    gateway: &GatewayConfig,
) -> Admission {
    let Some(descriptor) = shared.adapters.descriptor(owner, platform) else {
        return Admission::Dropped;
    };
    if super::operate::is_paused(shared, owner, platform) {
        tracing::debug!(profile = owner, platform, "dropped: the adapter is paused");
        return Admission::Paused;
    }
    if let Some(command) = command_alias(&descriptor, &inbound.text) {
        inbound.text = command;
    }
    let Some(store) = shared.store(owner) else {
        return Admission::Dropped;
    };
    if !super::admit_groups::authorized(
        shared,
        owner,
        platform,
        &store,
        &descriptor,
        &inbound,
        gateway,
    )
    .await
    {
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
    if !super::admit_groups::bot_admitted(shared, owner, platform, &inbound, gateway) {
        return Admission::Dropped;
    }
    if inbound.is_bot {
        // An admitted bot message (or a chunk of its burst) is a handoff:
        // `gateway.bots` already decided, group gating does not again.
        inbound.addressing.mentioned = true;
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
    if is_start_ping(&inbound) {
        reply(shared, owner, platform, &inbound, GREETING).await;
        return Admission::Handled;
    }
    let topics = shared
        .topics
        .enabled(&descriptor, owner, platform, &inbound.chat_id);
    if super::topics::in_lobby(topics, &inbound) {
        reply(shared, owner, platform, &inbound, super::topics::LOBBY).await;
        return Admission::Handled;
    }
    let inbound = match super::admit_groups::gate(shared, owner, platform, inbound, gateway).await {
        Ok(inbound) => inbound,
        Err(admission) => return admission,
    };
    if let Some(limited) = limit(shared, owner, platform, &inbound, gateway).await {
        return limited;
    }
    let mut inbound = inbound;
    let voice = match shared.agents.scope(owner) {
        Some(scope) => inbound::prepare(shared, owner, platform, &scope, &mut inbound).await,
        None => false,
    };
    let (routing, key) = super::profile_routes::routing(shared, owner, platform, &inbound, gateway);
    lane::input(shared, &key, routing, inbound, voice)
}

/// The routing of an admitted event and its conversation key.
pub(crate) fn routing_of(
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> (Routing, super::key::SessionKey) {
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
    (routing, key)
}

/// What a start ping answers.
pub(crate) const GREETING: &str = "Hi! Send me a message to start, or /help to see what I can do.";

/// A platform's start ping (spec: messaging-gateway, Platform start pings
/// and paginated command browser): Telegram's `/start` (with an optional
/// deep-link payload or `@bot` suffix), or an adapter's app-home open
/// submitted the same way. It is answered without a session or a turn and
/// is not counted against rate limits.
pub(crate) fn is_start_ping(inbound: &Inbound) -> bool {
    if inbound.callback.is_some() || !inbound.attachments.is_empty() {
        return false;
    }
    let first = inbound.text.split_whitespace().next().unwrap_or_default();
    first == "/start"
        || first
            .strip_prefix("/start@")
            .is_some_and(|bot| !bot.is_empty())
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
        thread_id: inbound.thread_id.as_deref(),
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

/// Whether `sender` would be admitted in a direct message on `platform`
/// of `profile` (the same order as an event's admission, without pairing
/// codes or side effects); any lookup failure denies.
pub(crate) async fn sender_admitted(
    shared: &Arc<Shared>,
    profile: &str,
    platform: &str,
    sender: &str,
) -> bool {
    let (Some(scope), Some(store), Some(descriptor)) = (
        shared.agents.scope(profile),
        shared.store(profile),
        shared.adapters.descriptor(profile, platform),
    ) else {
        return false;
    };
    let table = scope.config().gateway.platform(platform);
    let inbound = Inbound::text(super::key::ChatType::Dm, sender, sender, "");
    let mut facts = Facts {
        descriptor: &descriptor,
        config: &table,
        inbound: &inbound,
        paired: store.is_paired(platform, sender).unwrap_or(false),
        adapter: SenderVerdict::Unknown,
    };
    if auth::authorize(&facts).is_some() {
        return true;
    }
    if auth::asks_adapter(&descriptor)
        && let Some(adapter) = shared.adapters.adapter(profile, platform)
    {
        facts.adapter = auth::adapter_verdict(adapter.as_ref(), &inbound).await;
    }
    auth::authorize(&facts).is_some()
}

#[cfg(test)]
mod alias_tests {
    use super::*;

    fn slack() -> Descriptor {
        Descriptor {
            command_prefix_alias: Some("!".into()),
            ..Descriptor::new("slack", 39_000)
        }
    }

    #[test]
    fn a_bang_command_in_the_first_token_is_a_command() {
        assert_eq!(
            command_alias(&slack(), "<@U1> !stop").as_deref(),
            Some("/stop")
        );
        assert_eq!(
            command_alias(&slack(), "!model gpt").as_deref(),
            Some("/model gpt")
        );
        assert_eq!(command_alias(&slack(), "please run !stop later"), None);
        assert_eq!(command_alias(&slack(), "!! wow"), None);
        assert_eq!(
            command_alias(&Descriptor::new("discord", 2000), "!stop"),
            None
        );
    }
}
