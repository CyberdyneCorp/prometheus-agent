//! Request routing (spec: web-dashboard, SPA is embedded in the binary;
//! Host header validation; Public endpoints; Reverse-proxy base path from
//! config): the Host check, the base path, WebSocket upgrades, the public
//! endpoints (`/api/health`, `/login`, the sign-in callback), hashed
//! assets, the authenticated API, and the SPA shell for every other path.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request};

use crate::http::{Reply, Req, bytes, empty, error, redirect};
use crate::{Shared, api, assets, guard, login, ws};

/// Answers one request, with the security headers set.
pub(crate) async fn answer(
    shared: &Arc<Shared>,
    request: Request<Incoming>,
    peer: SocketAddr,
) -> Reply {
    guard::finish(route(shared, request, peer).await)
}

/// The path below the base path, or `None` outside it.
fn below_base<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(base)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

async fn route(shared: &Arc<Shared>, request: Request<Incoming>, peer: SocketAddr) -> Reply {
    let uri = request.uri().clone();
    let Some(path) = below_base(uri.path(), &shared.base) else {
        return crate::http::not_found();
    };
    if path.is_empty() {
        return redirect(&format!("{}/", shared.base));
    }
    let req = Req {
        method: request.method().clone(),
        path: path.to_owned(),
        query: uri.query().map(str::to_owned),
        headers: request.headers().clone(),
    };
    let host_ok = shared.guard.host_of(&req).is_some();
    if ws::is_upgrade(&request) {
        return ws::upgrade(shared, request, &req, host_ok);
    }
    if !host_ok {
        return bytes(400, "text/plain; charset=utf-8", "Invalid Host header");
    }
    let body = request.into_body();
    let ip = shared.guard.client_ip(&req, peer);
    match (&req.method, req.path.as_str()) {
        (&Method::GET, "/api/health") => api::system::health(shared),
        (&Method::GET, "/login") => login::get(shared, &req, peer).await,
        (&Method::POST, "/login") => login::post(shared, body, ip).await,
        (&Method::GET, "/auth/callback") => login::callback(shared, &req).await,
        (_, path) if path.starts_with("/assets/") => asset(&req),
        (_, path) if path == "/api" || path.starts_with("/api/") => {
            api::route(shared, &req, body).await
        }
        (_, path) if !shared.webapp.enabled && (path == "/app" || path.starts_with("/app/")) => {
            crate::http::not_found()
        }
        (&Method::GET | &Method::HEAD, _) => shell(shared),
        _ => error(
            405,
            "method_not_allowed",
            "only GET is served outside /api/",
        ),
    }
}

/// A hashed asset; a miss is a bare 404, never the SPA shell.
fn asset(req: &Req) -> Reply {
    let name = req.path.trim_start_matches("/assets/");
    match (req.method == Method::GET, assets::asset(name)) {
        (true, Some(asset)) => {
            let mut reply = bytes(200, asset.content_type, asset.bytes);
            reply.headers_mut().insert(
                hyper::header::CACHE_CONTROL,
                hyper::header::HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            reply
        }
        _ => empty(404),
    }
}

/// The SPA shell: every client-side route reloads into it.
fn shell(shared: &Shared) -> Reply {
    let mut reply = bytes(200, "text/html; charset=utf-8", assets::shell(&shared.base));
    reply.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    reply
}

#[cfg(test)]
mod tests {
    use super::below_base;

    #[test]
    fn paths_outside_the_base_are_not_served() {
        assert_eq!(below_base("/sessions/42", ""), Some("/sessions/42"));
        assert_eq!(below_base("/prometheus/", "/prometheus"), Some("/"));
        assert_eq!(below_base("/prometheus", "/prometheus"), Some(""));
        assert_eq!(below_base("/prometheusx/", "/prometheus"), None);
        assert_eq!(below_base("/other", "/prometheus"), None);
    }
}
