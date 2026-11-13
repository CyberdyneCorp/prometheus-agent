//! The authenticated JSON API (spec: web-dashboard, Loopback
//! authentication with an `HttpOnly` session cookie; CSRF protection on
//! state-changing requests; Profile-addressed routes).
//!
//! Every call needs the session cookie (`401` with the login hint
//! otherwise) and every non-GET call the session's CSRF token in
//! `X-Prometheus-CSRF` (`403 {"error": "csrf"}`). Machine-wide endpoints
//! live directly under `/api/`; everything that reads or writes profile
//! data lives under `/api/profiles/{profile}/` and never falls back to a
//! default profile.

mod config;
mod logs;
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
use crate::auth::sessions::{Signed, csrf_ok};
use crate::http::{Reply, Req, json_reply, not_found};
use crate::login;

/// The CSRF header of state-changing calls.
pub(crate) const CSRF_HEADER: &str = "x-prometheus-csrf";

/// Who is calling.
pub(crate) struct Caller {
    pub token: String,
    pub signed: Signed,
}

/// Answers an `/api/*` request (`/api/health` is answered before).
pub(crate) async fn route(shared: &Arc<Shared>, req: &Req, body: Incoming) -> Reply {
    let Some((token, signed)) = shared.auth.signed(req) else {
        return login::unauthorized();
    };
    let state_changing = !matches!(req.method, Method::GET | Method::HEAD);
    if state_changing && !csrf_ok(&signed, req.header(CSRF_HEADER)) {
        return json_reply(403, &json!({"error": "csrf"}));
    }
    let caller = Caller { token, signed };
    let segments = req.segments();
    match (&req.method, &segments[1..]) {
        (&Method::GET, ["session"]) => system::session(shared, &caller),
        (&Method::POST, ["logout"]) => login::logout(shared, &caller.token),
        (&Method::GET, ["status"]) => system::status(shared),
        (&Method::GET, ["profiles"]) => system::profiles(shared),
        (&Method::GET, ["system", "platform"]) => system::platform(shared),
        (&Method::GET, ["logs"]) => logs::tail(shared, req),
        (_, ["profiles", name, rest @ ..]) => {
            profile::route(shared, &caller, name, rest, req, body).await
        }
        _ => not_found(),
    }
}
