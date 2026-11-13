//! The sign-in pages (spec: web-dashboard, Loopback authentication with an
//! `HttpOnly` session cookie; Non-loopback binds fail closed; Public
//! endpoints): `GET /login` exchanges a one-time ticket for the session
//! cookie, or starts the configured remote sign-in; `POST /login` checks a
//! password; `GET /auth/callback` completes an OIDC sign-in. The pages are
//! plain HTML without scripts and carry no credential.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{HeaderValue, SET_COOKIE};
use pmt_config::daemon_dashboard::{AuthMode, PASSWORD_HASH_SECRET};
use serde_json::json;

use crate::Shared;
use crate::auth::{LOCAL, password, sessions};
use crate::http::{Reply, Req, bytes, json_reply, read_body, redirect};

/// The 401 body of every refused sign-in and unauthenticated API call.
pub(crate) fn unauthorized() -> Reply {
    json_reply(
        401,
        &json!({"error": "unauthorized", "login": "run 'prometheus dashboard' to get a login link"}),
    )
}

fn page(code: u16, title: &str, body: &str) -> Reply {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head><body><main><h1>{title}</h1>{body}</main></body></html>"
    );
    bytes(code, "text/html; charset=utf-8", html)
}

/// Signs `subject` in: the cookie and a redirect to the dashboard (or to
/// `next`, a path below the base path such as `/app`).
fn sign_in(shared: &Shared, subject: &str, next: Option<&str>) -> Reply {
    let role = shared.auth.role_for(subject);
    let (token, _) = shared.auth.sessions.create(subject, role);
    let next = next
        .filter(|n| n.starts_with('/') && !n.starts_with("//") && !n.contains('\\'))
        .unwrap_or("/");
    let mut reply = redirect(&format!("{}{next}", shared.base));
    let cookie = sessions::set_cookie(&token, shared.guard.secure_cookies());
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        reply.headers_mut().insert(SET_COOKIE, value);
    }
    reply
}

/// `GET /login`.
pub(crate) async fn get(shared: &Arc<Shared>, req: &Req, peer: SocketAddr) -> Reply {
    if let Some(ticket) = req.param("ticket") {
        if shared.auth.tickets.redeem(&ticket) {
            return sign_in(shared, LOCAL, req.param("next").as_deref());
        }
        return unauthorized();
    }
    match shared.auth.mode {
        AuthMode::Password => password_form(shared, None),
        AuthMode::Oidc | AuthMode::Portal => oidc_begin(shared, req, peer).await,
        AuthMode::None | AuthMode::Token => page(
            401,
            "Sign in to Prometheus",
            "<p>Run <code>prometheus dashboard</code> on this machine to get a login link.</p>",
        ),
    }
}

fn password_form(shared: &Shared, problem: Option<&str>) -> Reply {
    let problem = problem.map_or_else(String::new, |p| format!("<p role=\"alert\">{p}</p>"));
    page(
        if problem.is_empty() { 200 } else { 401 },
        "Sign in to Prometheus",
        &format!(
            "{problem}<form method=\"post\" action=\"{}/login\">\
             <label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\" required></label>\
             <button type=\"submit\">Sign in</button></form>",
            shared.base
        ),
    )
}

/// `POST /login`: a password sign-in, at most 5 wrong passwords a minute
/// per address.
pub(crate) async fn post(shared: &Arc<Shared>, body: Incoming, ip: IpAddr) -> Reply {
    if shared.auth.mode != AuthMode::Password {
        return crate::http::not_found();
    }
    if let Err(retry) = shared.auth.limiter.check(ip) {
        let mut reply = page(429, "Too many attempts", "<p>Try again later.</p>");
        if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
            reply.headers_mut().insert("retry-after", value);
        }
        return reply;
    }
    let Some(form) = read_body(body, 16 * 1024).await else {
        return page(413, "Too large", "");
    };
    let password = url::form_urlencoded::parse(&form)
        .find(|(k, _)| k == "password")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let launch = shared.host.launch_profile();
    let hash = shared
        .host
        .scope(&launch)
        .and_then(|scope| scope.secrets().resolve(PASSWORD_HASH_SECRET).ok());
    let verified = hash.is_some_and(|hash| password::verify(&password, hash.expose()));
    if verified {
        shared.auth.limiter.succeed(ip);
        return sign_in(shared, "password", None);
    }
    shared.auth.limiter.fail(ip);
    password_form(shared, Some("Wrong password."))
}

/// The OAuth callback URL (spec: web-dashboard, Additional dashboard
/// sign-in providers): `<dashboard.public_url>/auth/callback` when set,
/// else the origin the browser used (from a trusted proxy's
/// `X-Forwarded-Proto`/`X-Forwarded-Host`, or the validated `Host`).
pub(crate) fn redirect_uri(shared: &Shared, req: &Req, peer: SocketAddr) -> Option<String> {
    if let Some(public) = &shared.config.public_url {
        return Some(format!("{}/auth/callback", public.trim_end_matches('/')));
    }
    let origin = shared.guard.browser_origin(req, peer)?;
    Some(format!("{origin}{}/auth/callback", shared.base))
}

async fn oidc_begin(shared: &Shared, req: &Req, peer: SocketAddr) -> Reply {
    let (Some(oidc), Some(callback)) = (&shared.auth.oidc, redirect_uri(shared, req, peer)) else {
        return page(503, "Sign-in unavailable", "<p>OIDC is not configured.</p>");
    };
    match oidc.begin(&callback).await {
        Ok(url) => redirect(&url),
        Err(err) => {
            tracing::warn!("dashboard OIDC sign-in could not start: {err}");
            page(
                502,
                "Sign-in unavailable",
                "<p>The identity provider is unreachable.</p>",
            )
        }
    }
}

/// `GET /auth/callback`.
pub(crate) async fn callback(shared: &Arc<Shared>, req: &Req) -> Reply {
    let (Some(oidc), Some(code), Some(state)) =
        (&shared.auth.oidc, req.param("code"), req.param("state"))
    else {
        return unauthorized();
    };
    match oidc.complete(&code, &state).await {
        Ok(subject) => sign_in(shared, &subject, None),
        Err(err) => {
            tracing::warn!("dashboard OIDC sign-in refused: {err}");
            page(
                403,
                "Sign-in refused",
                "<p>This account may not use the dashboard.</p>",
            )
        }
    }
}

/// `POST /api/logout`.
pub(crate) fn logout(shared: &Shared, token: Option<&str>) -> Reply {
    if let Some(token) = token {
        shared.auth.sessions.remove(token);
    }
    let mut reply = json_reply(200, &json!({"ok": true}));
    if let Ok(value) = HeaderValue::from_str(&sessions::clear_cookie()) {
        reply.headers_mut().insert(SET_COOKIE, value);
    }
    reply
}
