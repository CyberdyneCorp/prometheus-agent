//! Dashboard plugin contributions (spec: web-dashboard, Dashboard plugin
//! contributions), for the profile named by `?profile=` (the launch
//! profile by default):
//!
//! - `GET /api/plugins`: the pages and slots of the profile's plugins in
//!   load order, with every override that lost to an earlier plugin;
//! - `POST /api/plugins/rescan`: reloads plugin discovery;
//! - `GET /api/plugin-frames/{id}`: the plugin's frame document, served
//!   with a `sandbox allow-scripts` policy (an opaque origin: no cookies,
//!   no storage of the dashboard's origin) that also forbids every network
//!   request, so the frame talks to the dashboard only through the plugin
//!   UI SDK's message channel, which this document carries;
//! - `/api/plugins/{id}/...`: the plugin's backend routes, answered by the
//!   daemon's plugin host process under the plugin's grants.
//!
//! Observers may read but not rescan or call a backend with anything but
//! `GET`.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use hyper::Method;
use hyper::body::Incoming;
use hyper::header::{CONTENT_SECURITY_POLICY, HeaderValue, X_FRAME_OPTIONS};
use serde_json::{Value, json};

use super::Caller;
use crate::Shared;
use crate::auth::sessions::Role;
use crate::http::{Reply, Req, bytes, error, from_error, json_reply, read_body};

/// The policy of a plugin frame document.
const FRAME_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; img-src data: blob:; font-src data:; form-action 'none'; \
    base-uri 'none'; frame-ancestors 'self'";
/// The largest backend request body forwarded.
const MAX_BODY: usize = 1024 * 1024;

/// The plugin UI SDK inside every frame: `prometheus.register()`,
/// `context()`, `api(method, path, body)` (the plugin's own backend) and
/// `navigate(path)`, each a promise answered by the dashboard.
const SDK: &str = r#"<script>(()=>{const p=new Map();let n=1;addEventListener("message",e=>{if(e.source!==parent)return;const m=e.data;if(!m||m.prometheus!==1||!p.has(m.id))return;const r=p.get(m.id);p.delete(m.id);m.error?r[1](new Error(m.error)):r[0](m.result)});const c=(method,params)=>new Promise((ok,no)=>{const id=n++;p.set(id,[ok,no]);parent.postMessage({prometheus:1,id,method,params},"*")});window.prometheus=Object.freeze({register:()=>c("register"),context:()=>c("context"),api:(method,path,body)=>c("api",{method,path,body}),navigate:path=>c("navigate",{path})})})();</script>"#;

/// The contributions of each profile, as last (re)scanned.
#[derive(Debug, Default)]
pub(crate) struct Scanned {
    profiles: Mutex<HashMap<String, Value>>,
}

/// The profile a call is for, if the caller may use it.
fn profile_of(shared: &Shared, caller: &Caller, req: &Req) -> Result<String, Reply> {
    let profile = req
        .param("profile")
        .unwrap_or_else(|| shared.host.launch_profile());
    let served = caller.signed.may_use(&profile) && shared.server.profiles().contains(&profile);
    if served {
        Ok(profile)
    } else {
        Err(json_reply(
            404,
            &json!({"error": format!("unknown profile {profile}")}),
        ))
    }
}

fn read_only(caller: &Caller) -> Option<Reply> {
    (caller.signed.role == Role::Observer)
        .then(|| error(403, "observer", "observers have read-only access"))
}

async fn scan(shared: &Shared, profile: &str) -> Result<Value, Reply> {
    let found = shared
        .host
        .dashboard_plugins(profile)
        .await
        .map_err(|e| from_error(&e))?;
    shared
        .plugins
        .profiles
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(profile.to_owned(), found.clone());
    Ok(found)
}

/// `GET /api/plugins`.
pub(crate) async fn list(shared: &Shared, caller: &Caller, req: &Req) -> Reply {
    let profile = match profile_of(shared, caller, req) {
        Ok(profile) => profile,
        Err(reply) => return reply,
    };
    let cached = shared
        .plugins
        .profiles
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&profile)
        .cloned();
    let found = match cached {
        Some(found) => found,
        None => match scan(shared, &profile).await {
            Ok(found) => found,
            Err(reply) => return reply,
        },
    };
    json_reply(200, &found)
}

/// `POST /api/plugins/rescan`.
pub(crate) async fn rescan(shared: &Shared, caller: &Caller, req: &Req) -> Reply {
    if let Some(refused) = read_only(caller) {
        return refused;
    }
    let profile = match profile_of(shared, caller, req) {
        Ok(profile) => profile,
        Err(reply) => return reply,
    };
    if let Err(err) = shared.host.rescan_plugins(&profile).await {
        return from_error(&err);
    }
    match scan(shared, &profile).await {
        Ok(found) => json_reply(200, &found),
        Err(reply) => reply,
    }
}

/// The plugin's document with the SDK first in its head.
fn with_sdk(document: &str) -> String {
    let lower = document.to_ascii_lowercase();
    match lower.find("<head>") {
        Some(at) => {
            let end = at + "<head>".len();
            format!("{}{SDK}{}", &document[..end], &document[end..])
        }
        None => format!("<!doctype html><meta charset=\"utf-8\">{SDK}{document}"),
    }
}

/// `GET /api/plugin-frames/{id}`.
pub(crate) async fn frame(shared: &Shared, caller: &Caller, req: &Req, id: &str) -> Reply {
    let profile = match profile_of(shared, caller, req) {
        Ok(profile) => profile,
        Err(reply) => return reply,
    };
    match shared.host.plugin_frame(&profile, id).await {
        Ok(document) => {
            let mut reply = bytes(200, "text/html; charset=utf-8", with_sdk(&document));
            let headers = reply.headers_mut();
            headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(FRAME_CSP));
            headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
            headers.insert(
                hyper::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            reply
        }
        Err(err) => from_error(&err),
    }
}

/// `/api/plugins/{id}/{rest...}`: a backend route of the plugin.
pub(crate) async fn backend(
    shared: &Shared,
    caller: &Caller,
    req: &Req,
    id: &str,
    rest: &[&str],
    body: Incoming,
) -> Reply {
    if req.method != Method::GET
        && req.method != Method::HEAD
        && let Some(refused) = read_only(caller)
    {
        return refused;
    }
    let profile = match profile_of(shared, caller, req) {
        Ok(profile) => profile,
        Err(reply) => return reply,
    };
    let Some(raw) = read_body(body, MAX_BODY).await else {
        return error(413, "body_too_large", "the request body is too large");
    };
    let text = String::from_utf8_lossy(&raw).into_owned();
    let body = serde_json::from_str::<Value>(&text).unwrap_or(if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    });
    let request = json!({
        "kind": "http",
        "method": req.method.as_str(),
        "path": format!("/{}", rest.join("/")),
        "query": req.query.clone().unwrap_or_default(),
        "profile": profile,
        "role": if caller.signed.role == Role::Observer { "observer" } else { "driver" },
        "body": body,
    });
    match shared.host.plugin_request(&profile, id, request).await {
        Ok(answer) => {
            let status = answer["status"]
                .as_u64()
                .and_then(|s| u16::try_from(s).ok())
                .filter(|s| (200..600).contains(s))
                .unwrap_or(502);
            json_reply(status, &answer["body"])
        }
        Err(err) => from_error(&err),
    }
}

#[cfg(test)]
mod tests {
    use super::with_sdk;

    #[test]
    fn the_sdk_comes_first_in_the_head() {
        let page =
            with_sdk("<!DOCTYPE html><html><HEAD><title>x</title></head><body>hi</body></html>");
        assert!(
            page.starts_with("<!DOCTYPE html><html><HEAD><script>"),
            "{page}"
        );
        assert!(page.ends_with("<title>x</title></head><body>hi</body></html>"));
        let bare = with_sdk("<p>hi</p>");
        assert!(bare.starts_with("<!doctype html><meta charset=\"utf-8\"><script>"));
        assert!(bare.ends_with("<p>hi</p>"));
    }
}
