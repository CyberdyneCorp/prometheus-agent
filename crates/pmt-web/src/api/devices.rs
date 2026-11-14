//! Device pairing over the dashboard listener (spec: web-dashboard, Paired
//! device credentials for remote clients): `POST /api/devices/pair`
//! exchanges a one-use pairing code (`prometheus dashboard pair`) for the
//! device's credential, and `POST /api/devices/refresh` rotates the
//! calling device's credential. A non-loopback listener only runs with
//! TLS, so neither travels in clear text off the machine.

use std::sync::Arc;

use hyper::Method;
use hyper::body::Incoming;
use serde_json::json;

use crate::Shared;
use crate::auth::bearer_of;
use crate::http::{Reply, Req, error, from_error, json_reply, not_found, read_json};

/// Answers a device endpoint.
pub(crate) async fn route(shared: &Arc<Shared>, req: &Req, body: Incoming) -> Reply {
    match (&req.method, req.path.as_str()) {
        (&Method::POST, "/api/devices/pair") => pair(shared, body).await,
        (&Method::POST, "/api/devices/refresh") => refresh(shared, req),
        _ => not_found(),
    }
}

async fn pair(shared: &Shared, body: Incoming) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let Some(code) = body["code"].as_str() else {
        return error(
            400,
            "invalid_body",
            "expected {\"code\": \"<pairing code>\"}",
        );
    };
    match shared.auth.devices.redeem_code(code) {
        Ok(Some((device, token))) => json_reply(200, &json!({"device": device, "token": token})),
        Ok(None) => error(
            401,
            "pairing_code",
            "the pairing code is unknown, used or expired",
        ),
        Err(err) => from_error(&err),
    }
}

fn refresh(shared: &Shared, req: &Req) -> Reply {
    let Some(token) = bearer_of(req) else {
        return crate::login::unauthorized();
    };
    match shared.auth.devices.refresh(token) {
        Ok(Some(token)) => json_reply(200, &json!({"token": token})),
        Ok(None) => crate::login::unauthorized(),
        Err(err) => from_error(&err),
    }
}
