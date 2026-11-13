//! Machine-wide endpoints (spec: web-dashboard, Public endpoints; Built-in
//! pages ("Status page exists"); Honest platform support matrix).

use std::sync::Arc;

use serde_json::{Value, json};

use super::Caller;
use crate::Shared;
use crate::http::{Reply, json_reply};

/// `GET /api/health`: public, reads no profile data.
pub(crate) fn health(shared: &Arc<Shared>) -> Reply {
    let identity = shared.server.identity();
    json_reply(
        200,
        &json!({
            "ok": true,
            "version": identity.version,
            "build_id": identity.build_id,
            "protocol_version": shared.server.protocol().to_string(),
            "auth_required": true,
            "auth_providers": shared.auth.providers(),
        }),
    )
}

/// `GET /api/session`: the signed-in browser (the SPA reads its CSRF token
/// here; served pages never carry it).
pub(crate) fn session(shared: &Shared, caller: &Caller) -> Reply {
    json_reply(
        200,
        &json!({
            "subject": caller.signed.subject,
            "role": caller.signed.role.as_str(),
            "csrf": caller.signed.csrf,
            "auth_mode": shared.auth.mode.as_str(),
            "base_path": shared.base,
            "max_upload_bytes": shared.config.max_upload_bytes,
        }),
    )
}

/// `GET /api/status`: daemon version, uptime, clients, sessions, adapters
/// and the platform matrix.
pub(crate) fn status(shared: &Shared) -> Reply {
    let status = shared.server.status();
    let adapters: Vec<Value> = status
        .adapters
        .iter()
        .map(|a| {
            json!({"profile": a.profile, "platform": a.platform, "state": a.state,
                   "message": a.message, "paused": a.paused})
        })
        .collect();
    json_reply(
        200,
        &json!({
            "version": status.version,
            "build_id": status.build_id,
            "uptime_secs": status.uptime_secs,
            "clients": status.connections,
            "sessions": status.sessions,
            "active_turns": status.active_turns,
            "profiles": status.profiles,
            "adapters": adapters,
            "health": status.health.iter().map(|h| json!({
                "component": h.component, "profile": h.profile, "state": h.state, "reason": h.reason,
            })).collect::<Vec<_>>(),
            "platform": shared.server.platform_support(),
        }),
    )
}

/// `GET /api/profiles`.
pub(crate) fn profiles(shared: &Shared) -> Reply {
    let profiles: Vec<Value> = shared
        .server
        .profiles()
        .into_iter()
        .map(|name| json!({"name": name}))
        .collect();
    json_reply(200, &json!({"profiles": profiles}))
}

/// `GET /api/system/platform`.
pub(crate) fn platform(shared: &Shared) -> Reply {
    json_reply(200, &json!(shared.server.platform_support()))
}
