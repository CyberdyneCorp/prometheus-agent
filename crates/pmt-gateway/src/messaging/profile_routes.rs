//! Shared-credential profile routes at admission (spec: messaging-gateway,
//! Shared-credential profile routes). One profile's adapter (one bot
//! token) can serve several communities: an authorized event matching one
//! of the receiving profile's `gateway.profile_routes` goes to the route's
//! target profile when that profile is served and lists the receiving one
//! in its `gateway.accept_routes_from`. The target's session keys, config,
//! memory and toolsets then apply, while replies still leave through the
//! receiving profile's adapter (the target borrows it). Unmatched events,
//! and events for a target that does not accept them, stay with the
//! receiving profile.

use pmt_config::schema::{GatewayConfig, RouteFacts, resolve_route};

use super::Shared;
use super::adapter::Inbound;
use super::key::{Isolation, Routing, SessionKey};

/// The profile `owner`'s event on `platform` belongs to (`owner` itself
/// unless a route sends it elsewhere).
fn target(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> Option<(String, GatewayConfig)> {
    let facts = RouteFacts {
        platform,
        guild_id: inbound.scope_id.as_deref(),
        chat_id: &inbound.chat_id,
        thread_id: inbound.thread_id.as_deref(),
        user_id: &inbound.sender_id,
    };
    let route = resolve_route(&gateway.profile_routes, &facts)?;
    let Some(scope) = shared.agents.scope(&route.profile) else {
        tracing::warn!(
            profile = owner,
            platform,
            route = route.name,
            "profile route to '{}', which this daemon does not serve; the event stays",
            route.profile
        );
        return None;
    };
    let target = scope.config().gateway.clone();
    if !target.accept_routes_from.iter().any(|p| p == owner) {
        tracing::warn!(
            profile = owner,
            platform,
            route = route.name,
            "profile '{}' does not accept routes from '{owner}'; the event stays",
            route.profile
        );
        return None;
    }
    Some((route.profile.clone(), target))
}

/// The routing and conversation key of an event `owner`'s adapter on
/// `platform` admitted: under the target profile when a profile route
/// sends it there (the receiving adapter is lent to it for replies).
pub(crate) fn routing(
    shared: &Shared,
    owner: &str,
    platform: &str,
    inbound: &Inbound,
    gateway: &GatewayConfig,
) -> (Routing, SessionKey) {
    let Some((profile, config)) = target(shared, owner, platform, inbound, gateway) else {
        return super::admit::routing_of(owner, platform, inbound, gateway);
    };
    shared.adapters.lend(&profile, platform, owner);
    let (group_per_user, thread_per_user) = config.sessions_for(platform);
    let routing = Routing {
        adapter: format!("{owner}/{platform}"),
        profile,
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

/// Lends every receiving adapter to the targets its profile's routes
/// name (a restarted daemon delivers a routed session's queued replies
/// before any new event arrives).
pub(crate) fn lend_configured(shared: &Shared) {
    for owner in shared.server.profiles() {
        let Some(scope) = shared.agents.scope(&owner) else {
            continue;
        };
        for route in &scope.config().gateway.profile_routes {
            let accepted = shared
                .agents
                .scope(&route.profile)
                .is_some_and(|s| s.config().gateway.accept_routes_from.contains(&owner));
            if accepted {
                shared
                    .adapters
                    .lend(&route.profile, &route.platform, &owner);
            }
        }
    }
}
