//! Request routing: the browser and Host guard first, the unauthenticated
//! health check, then the routed profile's bearer check and the endpoint.

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
    let Some(req) = Req::read(request).await else {
        let refused = error(
            413,
            "invalid_request_error",
            "body_too_large",
            "the request body is too large",
        );
        return Guard::finish(refused, None);
    };
    let origin = match shared.guard.admit(&req) {
        Ok(origin) => origin,
        Err(refused) => return Guard::finish(refused, None),
    };
    if req.method == Method::OPTIONS {
        return match &origin {
            Some(origin) => Guard::finish(Guard::preflight(origin), None),
            None => Guard::finish(not_found("the route"), None),
        };
    }
    let reply = route(shared, &req).await;
    Guard::finish(reply, origin.as_deref())
}

async fn route(shared: &Arc<Shared>, req: &Req) -> Reply {
    let (profile, path) = auth::split(&req.path);
    if req.method == Method::GET && (path == "/health" || path == "/v1/health") {
        return json_reply(200, &json!({"status": "ok"}));
    }
    if !shared.server.profiles().contains(&profile) {
        return not_found("the route");
    }
    let caller = match auth::authenticate(shared.host.as_ref(), &profile, req) {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let caller = &caller;
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
