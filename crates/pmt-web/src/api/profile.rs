//! `/api/profiles/{profile}/...` (spec: web-dashboard, Profile-addressed
//! routes): the profile must be one the daemon serves (`404 {"error":
//! "unknown profile <name>"}` otherwise), and every handler works on that
//! profile's own scope and store only. Observers (remote users not in
//! `dashboard.auth.drivers`) are read-only: every state-changing route is
//! refused here with `403 {"error": "observer"}` (spec: web-dashboard,
//! Remote dashboard users are observers by default), so routes added
//! later inherit the refusal.

use std::sync::Arc;

use hyper::Method;
use hyper::body::Incoming;
use serde_json::json;

use super::{Caller, config, secrets, sessions, uploads, usage};
use crate::Shared;
use crate::auth::sessions::Role;
use crate::host::Catalog;
use crate::http::{Reply, Req, error, from_error, json_reply, not_found};

pub(crate) async fn route(
    shared: &Arc<Shared>,
    caller: &Caller,
    profile: &str,
    rest: &[&str],
    req: &Req,
    body: Incoming,
) -> Reply {
    let served = shared.server.profiles().iter().any(|p| p == profile);
    let (Some(scope), Some(store)) = (
        served.then(|| shared.host.scope(profile)).flatten(),
        served.then(|| shared.host.store(profile)).flatten(),
    ) else {
        return json_reply(404, &json!({"error": format!("unknown profile {profile}")}));
    };
    let reads = matches!(req.method, Method::GET | Method::HEAD);
    if !reads && caller.signed.role == Role::Observer {
        return error(403, "observer", "observers have read-only access");
    }
    let at = Profile {
        name: profile,
        scope: &scope,
        store: &store,
    };
    match (&req.method, rest) {
        (&Method::GET, ["sessions"]) => sessions::list(&at, req),
        (&Method::GET, ["sessions", id, "messages"]) => sessions::messages(&at, id),
        (&Method::DELETE, ["sessions", id]) => sessions::delete(shared, &at, id),
        (&Method::GET, ["config"]) => config::get(&at),
        (&Method::PATCH, ["config"]) => config::patch(&at, body).await,
        (&Method::GET, ["config", "schema"]) => config::schema(),
        (&Method::GET, ["config", "defaults"]) => config::defaults(),
        (&Method::GET, ["secrets"]) => secrets::list(&at),
        (&Method::PUT, ["secrets", name]) => secrets::put(&at, name, body).await,
        (&Method::DELETE, ["secrets", name]) => secrets::delete(&at, name),
        (&Method::GET, ["usage"]) => usage::report(&at, req),
        (&Method::GET, ["usage.csv"]) => usage::csv(&at, req),
        (&Method::GET, ["skills"]) => catalog(shared, profile, Catalog::Skills).await,
        (&Method::GET, ["toolsets"]) => catalog(shared, profile, Catalog::Toolsets).await,
        (&Method::GET, ["models"]) => catalog(shared, profile, Catalog::Models).await,
        (&Method::GET, ["plugins"]) => catalog(shared, profile, Catalog::Plugins).await,
        (&Method::GET, ["mcp"]) => catalog(shared, profile, Catalog::Mcp).await,
        (&Method::GET, ["cron"]) => catalog(shared, profile, Catalog::Cron).await,
        (&Method::GET, ["memory"]) => catalog(shared, profile, Catalog::Memory).await,
        (&Method::POST, ["uploads"]) => uploads::upload(shared, &at, req, body).await,
        (&Method::GET, ["blobs", sha]) => uploads::download(&at, sha),
        _ => not_found(),
    }
}

/// The addressed profile.
pub(crate) struct Profile<'a> {
    pub name: &'a str,
    pub scope: &'a pmt_config::ProfileScope,
    pub store: &'a pmt_store::Store,
}

async fn catalog(shared: &Shared, profile: &str, what: Catalog) -> Reply {
    match shared.host.catalog(profile, what).await {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}
