//! Request routing: the browser and Host guard first, the unauthenticated
//! health check, then the routed profile's bearer check, the body (only
//! for an authenticated caller) and the endpoint.

use std::sync::Arc;

use hyper::Method;
use hyper::body::Incoming;
use serde_json::json;

use crate::Shared;
use crate::guard::Guard;
use crate::host::Catalog;
use crate::http::{Reply, Req, error, json_reply, not_found};
use crate::{auth, chat, discovery, responses, runs, sessions_api};

/// Answers one request.
pub(crate) async fn answer(shared: &Arc<Shared>, request: hyper::Request<Incoming>) -> Reply {
    let (parts, body) = request.into_parts();
    let head = Req::head(parts);
    let origin = match shared.guard.admit(&head) {
        Ok(origin) => origin,
        Err(refused) => return Guard::finish(refused, None),
    };
    if head.method == Method::OPTIONS {
        return match &origin {
            Some(origin) => Guard::finish(Guard::preflight(origin), None),
            None => Guard::finish(not_found("the route"), None),
        };
    }
    let reply = route(shared, head, body).await;
    Guard::finish(reply, origin.as_deref())
}

/// Routes a request whose head passed the guard. The body is read only
/// once the caller is authenticated, so an anonymous client never makes
/// the daemon buffer an upload.
async fn route(shared: &Arc<Shared>, head: Req, body: Incoming) -> Reply {
    let (profile, path) = auth::split(&head.path);
    if head.method == Method::GET && (path == "/health" || path == "/v1/health") {
        return json_reply(200, &json!({"status": "ok"}));
    }
    if !shared.server.profiles().contains(&profile) {
        return not_found("the route");
    }
    let caller = match auth::authenticate(shared.host.as_ref(), &profile, &head) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let Some(req) = head.with_body(body).await else {
        return error(
            413,
            "invalid_request_error",
            "body_too_large",
            "the request body is too large",
        );
    };
    endpoint(shared, &caller, &path, &req).await
}

async fn endpoint(shared: &Arc<Shared>, caller: &auth::Caller, path: &str, req: &Req) -> Reply {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (req.method.clone(), segments.as_slice()) {
        (Method::GET, ["v1", "models"]) => discovery::models(shared, caller),
        (Method::GET, ["v1", "capabilities"]) => discovery::capabilities(shared, caller),
        (Method::GET, ["v1", "skills"]) => {
            discovery::catalog(shared, caller, Catalog::Skills).await
        }
        (Method::GET, ["v1", "toolsets"]) => {
            discovery::catalog(shared, caller, Catalog::Toolsets).await
        }
        (Method::GET, ["api", "model", "options"]) => {
            discovery::model_options(shared, caller).await
        }
        (Method::GET, ["health", "detailed"]) => discovery::health(shared, caller).await,
        (Method::POST, ["v1", "chat", "completions"]) => chat::complete(shared, caller, req).await,
        (Method::POST, ["v1", "responses"]) => responses::create(shared, caller, req).await,
        (Method::GET, ["v1", "responses", id]) => responses::get(shared, caller, id),
        (Method::DELETE, ["v1", "responses", id]) => responses::delete(shared, caller, id),
        (Method::POST, ["v1", "runs"]) => runs::create(shared, caller, req).await,
        (Method::GET, ["v1", "runs", id]) => runs::get(shared, caller, id),
        (Method::GET, ["v1", "runs", id, "events"]) => runs::events(shared, caller, id, req),
        (Method::POST, ["v1", "runs", id, action]) => {
            runs::control(shared, caller, id, action, req).await
        }
        (_, ["api", "sessions", rest @ ..]) => sessions_api::route(shared, caller, rest, req).await,
        (_, ["api", "jobs", rest @ ..]) => discovery::jobs(shared, caller, rest, req).await,
        _ => not_found("the route"),
    }
}
