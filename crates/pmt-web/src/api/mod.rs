//! The authenticated JSON API (spec: web-dashboard, Loopback
//! authentication with an `HttpOnly` session cookie; CSRF protection on
//! state-changing requests; Profile-addressed routes).
//!
//! Only endpoints of the inventory ([`crate::inventory`]) are served:
//! anything else answers 404. Every call needs the session cookie or a
//! bearer credential (`401` with the login hint otherwise) and every
//! non-GET cookie call the session's CSRF token in `X-Prometheus-CSRF`
//! (`403 {"error": "csrf"}`); bearer credentials are not ambient, so they
//! need none. Machine-wide endpoints live directly under `/api/`;
//! everything that reads or writes profile data lives under
//! `/api/profiles/{profile}/` and never falls back to a default profile.

pub(crate) mod adapter_actions;
mod channels;
mod config;
pub(crate) mod devices;
mod files;
mod logs;
mod maintenance;
mod messaging;
pub(crate) mod plugins;
mod profile;
mod secrets;
mod sessions;
pub(crate) mod system;
mod uploads;
mod usage;

use std::sync::Arc;

use hyper::Method;
use hyper::body::Incoming;
use serde_json::json;

use crate::Shared;
use crate::auth::sessions::{Signed, Via, csrf_ok};
use crate::http::{Reply, Req, json_reply, not_found};
use crate::inventory::{self, Access};
use crate::login;

/// The CSRF header of state-changing calls.
pub(crate) const CSRF_HEADER: &str = "x-prometheus-csrf";

/// Who is calling.
pub(crate) struct Caller {
    /// The browser's cookie token (`None` for bearer callers).
    pub token: Option<String>,
    pub signed: Signed,
}

/// The caller of `req`, by cookie or bearer credential.
pub(crate) fn caller(shared: &Shared, req: &Req) -> Option<Caller> {
    let deployment = || {
        let launch = shared.host.launch_profile();
        let scope = shared.host.scope(&launch)?;
        let secret = scope
            .secrets()
            .resolve(pmt_config::daemon_dashboard::SESSION_TOKEN_SECRET)
            .ok()?;
        Some(secret.expose().to_owned())
    };
    let (token, signed) = shared.auth.caller(req, deployment)?;
    Some(Caller { token, signed })
}

/// Answers an `/api/*` request (`/api/health` is answered before).
pub(crate) async fn route(shared: &Arc<Shared>, req: &Req, body: Incoming) -> Reply {
    let Some(endpoint) = inventory::find(&req.method, &req.path) else {
        return not_found();
    };
    if endpoint.access == Access::Device {
        return devices::route(shared, req, body).await;
    }
    let Some(caller) = caller(shared, req) else {
        return login::unauthorized();
    };
    let state_changing = !matches!(req.method, Method::GET | Method::HEAD);
    if state_changing
        && caller.signed.via == Via::Cookie
        && !csrf_ok(&caller.signed, req.header(CSRF_HEADER))
    {
        return json_reply(403, &json!({"error": "csrf"}));
    }
    let segments = req.segments();
    match (&req.method, &segments[1..]) {
        (&Method::GET, ["session"]) => system::session(shared, &caller),
        (&Method::POST, ["logout"]) => login::logout(shared, caller.token.as_deref()),
        (&Method::GET, ["status"]) => system::status(shared),
        (&Method::GET, ["profiles"]) => system::profiles(shared),
        (&Method::GET, ["system", "platform"]) => system::platform(shared),
        (&Method::GET, ["docs"]) => crate::docs::index(),
        (&Method::GET, ["docs", "page"]) => crate::docs::page(req),
        (&Method::GET, ["docs", "search"]) => crate::docs::search(req),
        (&Method::GET, ["themes"]) => crate::themes::index(shared.server.home()),
        (&Method::GET, ["themes", name, "css"]) => crate::themes::css(shared.server.home(), name),
        (&Method::POST, ["profiles"]) => system::create_profile(shared, &caller, body).await,
        (&Method::GET, ["plugins"]) => plugins::list(shared, &caller, req).await,
        (&Method::POST, ["plugins", "rescan"]) => plugins::rescan(shared, &caller, req).await,
        (&Method::GET, ["plugin-frames", id]) => plugins::frame(shared, &caller, req, id).await,
        (_, ["plugins", id, rest @ ..]) => {
            plugins::backend(shared, &caller, req, id, rest, body).await
        }
        (&Method::GET, ["logs"]) => logs::tail(shared, &caller, req),
        (&Method::GET, ["events" | "rpc"]) => {
            crate::http::error(426, "upgrade_required", "this endpoint is a WebSocket")
        }
        (_, ["profiles", name, rest @ ..]) => {
            profile::route(shared, &caller, name, rest, req, body).await
        }
        _ => not_found(),
    }
}
