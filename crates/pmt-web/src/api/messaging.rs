//! A profile's messaging adapters and pairings over the dashboard (spec:
//! web-dashboard, REST endpoint inventory): adapter pause and resume, and the messaging pairing list with code approval, through the
//! daemon's messaging gateway (`503` when it does not run).

use hyper::body::Incoming;
use pmt_gateway::messaging::Gateway;
use serde_json::{Value, json};

use super::profile::Profile;
use crate::Shared;
use crate::http::{Reply, error, from_error, json_reply, read_json};

fn gateway(shared: &Shared) -> Result<&Gateway, Reply> {
    shared
        .server
        .messaging()
        .ok_or_else(|| error(503, "messaging_off", "the messaging gateway is not running"))
}

/// `POST adapters/{name}/pause|resume`.
pub(crate) async fn pause_or_resume(
    shared: &Shared,
    profile: &str,
    name: &str,
    verb: &str,
) -> Reply {
    let gateway = match gateway(shared) {
        Ok(gateway) => gateway,
        Err(reply) => return reply,
    };
    let done = match verb {
        "pause" => gateway
            .pause_platform(profile, name, "dashboard")
            .map(|changed| json!({"platform": name, "paused": true, "changed": changed})),
        "resume" => gateway
            .resume_platform(profile, name)
            .await
            .map(|status| json!({"platform": name, "paused": false, "state": status.state})),
        _ => return crate::http::not_found(),
    };
    match done {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}

/// `GET pairing`.
pub(crate) fn pairings(at: &Profile<'_>) -> Reply {
    match at.store.pairings(None) {
        Ok(list) => {
            let rows: Vec<Value> = list
                .iter()
                .map(|p| {
                    json!({"platform": p.platform, "sender_id": p.sender_id, "sender_name": p.sender_name,
                           "approved": p.approved, "requested_at": p.requested_at.to_string(),
                           "expires_at": p.expires_at.map(|t| t.to_string())})
                })
                .collect();
            json_reply(200, &json!({"pairings": rows}))
        }
        Err(err) => from_error(&err),
    }
}

/// `POST pairing/{code} {"platform": "telegram"}`.
pub(crate) async fn approve(shared: &Shared, profile: &str, code: &str, body: Incoming) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let Some(platform) = body["platform"].as_str() else {
        return error(
            400,
            "invalid_body",
            "expected {\"platform\": \"<platform>\"}",
        );
    };
    let gateway = match gateway(shared) {
        Ok(gateway) => gateway,
        Err(reply) => return reply,
    };
    match gateway.approve_pairing(profile, platform, code) {
        Ok(p) => json_reply(
            200,
            &json!({"platform": p.platform, "sender_id": p.sender_id, "approved": p.approved}),
        ),
        Err(err) => from_error(&err),
    }
}
