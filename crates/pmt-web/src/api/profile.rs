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

use super::{
    Caller, adapter_actions, channels, config, files, maintenance, messaging, secrets, sessions,
    uploads, usage,
};
use crate::Shared;
use crate::auth::sessions::Role;
use crate::host::{Catalog, Change};
use crate::http::{Reply, Req, error, from_error, json_reply, not_found, read_json};

pub(crate) async fn route(
    shared: &Arc<Shared>,
    caller: &Caller,
    profile: &str,
    rest: &[&str],
    req: &Req,
    body: Incoming,
) -> Reply {
    let served =
        caller.signed.may_use(profile) && shared.server.profiles().iter().any(|p| p == profile);
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
    if let Some((what, target)) = change_of(&req.method, rest) {
        return change(shared, profile, what, target, body).await;
    }
    match (&req.method, rest) {
        (&Method::GET, ["sessions"]) => sessions::list(&at, req),
        (&Method::GET, ["sessions", id, "messages"]) => sessions::messages(&at, id),
        (&Method::GET, ["sessions", id, "file"]) => {
            sessions::host_file(shared, profile, id, req).await
        }
        (&Method::DELETE, ["sessions", id]) => sessions::delete(shared, &at, id),
        (&Method::GET, ["config"]) => config::get(&at),
        (&Method::PATCH, ["config"]) => config::patch(&at, body).await,
        (&Method::GET, ["config", "schema"]) => config::schema(),
        (&Method::GET, ["config", "defaults"]) => config::defaults(),
        (&Method::GET, ["secrets"]) => secrets::list(&at),
        (&Method::GET, ["secrets", name]) => secrets::show(&at, name),
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
        (&Method::GET, ["webhooks"]) => catalog(shared, profile, Catalog::Webhooks).await,
        (&Method::GET, ["adapters"]) => channels::list(shared, &at),
        (&Method::PUT, ["adapters", name]) => channels::save(&at, name, body).await,
        (&Method::GET, ["files"]) => files::list_roots(&at),
        (&Method::GET, ["files", "list"]) => files::list(shared, &at, req),
        (&Method::GET, ["files", "read"]) => files::read(shared, &at, req),
        (&Method::POST, ["files", "upload"]) => files::upload(shared, &at, req, body).await,
        (&Method::POST, ["files", "rename"]) => files::rename(shared, &at, req, body).await,
        (&Method::DELETE, ["files"]) => files::delete(shared, &at, req),
        (&Method::POST, ["adapters", name, "test"]) => {
            adapter_actions::test(shared, profile, name).await
        }
        (&Method::POST, ["adapters", name, "restart"]) => {
            adapter_actions::restart(shared, profile, name).await
        }
        (&Method::POST, ["adapters", name, "pairing"]) => {
            adapter_actions::start(shared, profile, name)
        }
        (&Method::GET, ["adapters", name, "pairing"]) => {
            adapter_actions::state(shared, profile, name)
        }
        (&Method::DELETE, ["adapters", name, "pairing"]) => {
            adapter_actions::cancel(shared, profile, name)
        }
        (&Method::POST, ["adapters", name, verb]) => {
            messaging::pause_or_resume(shared, profile, name, verb).await
        }
        (&Method::GET, ["pairing"]) => messaging::pairings(&at),
        (&Method::POST, ["pairing", code]) => messaging::approve(shared, profile, code, body).await,
        (&Method::POST, ["maintenance", action]) => {
            maintenance::run(shared, profile, action, body).await
        }
        (&Method::POST, ["uploads"]) => uploads::upload(shared, &at, req, body).await,
        (&Method::POST, ["stage"]) => uploads::stage(shared, &at, req, body).await,
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

/// The host change a route asks for, with its target.
fn change_of<'a>(method: &Method, rest: &[&'a str]) -> Option<(Change, Option<&'a str>)> {
    let found = match (method, rest) {
        (&Method::PATCH, ["skills", name]) => (Change::Skill, Some(*name)),
        (&Method::PATCH, ["toolsets", name]) => (Change::Toolset, Some(*name)),
        (&Method::POST, ["cron"]) => (Change::CronCreate, None),
        (&Method::PATCH, ["cron", id]) => (Change::CronUpdate, Some(*id)),
        (&Method::DELETE, ["cron", id]) => (Change::CronDelete, Some(*id)),
        (&Method::POST, ["cron", id, "run"]) => (Change::CronRun, Some(*id)),
        (&Method::PATCH, ["memory"]) => (Change::Memory, None),
        (&Method::POST, ["webhooks"]) => (Change::WebhookCreate, None),
        (&Method::PATCH, ["webhooks", name]) => (Change::WebhookUpdate, Some(*name)),
        (&Method::DELETE, ["webhooks", name]) => (Change::WebhookDelete, Some(*name)),
        (&Method::POST, ["webhooks", name, "test"]) => (Change::WebhookTest, Some(*name)),
        _ => return None,
    };
    Some(found)
}

async fn change(
    shared: &Shared,
    profile: &str,
    what: Change,
    target: Option<&str>,
    body: Incoming,
) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    match shared.host.change(profile, what, target, body).await {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}

async fn catalog(shared: &Shared, profile: &str, what: Catalog) -> Reply {
    match shared.host.catalog(profile, what).await {
        Ok(value) => json_reply(200, &value),
        Err(err) => from_error(&err),
    }
}
