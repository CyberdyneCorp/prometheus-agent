//! Browser access, Host checks and security headers (spec: api-server,
//! Browser access, Host checks and security headers). A request carrying
//! an `Origin` outside `api_server.cors_origins` is refused before any
//! work, as is one whose `Host` is not a loopback name (or the configured
//! `public_host`) on a loopback bind: a page that rebinds its DNS name to
//! 127.0.0.1 cannot reach the server. Every answer says `nosniff` and
//! `no-referrer`; CORS headers go only to allowlisted origins.

use pmt_config::daemon_api::ApiListener;

use crate::http::{Reply, Req, error, full, with_header};

/// The request headers a browser may send.
const ALLOWED_HEADERS: &str = "Authorization, Content-Type, Idempotency-Key, X-Prometheus-Session-Id, X-Prometheus-Session-Key";
/// The answer headers a browser may read.
const EXPOSED_HEADERS: &str =
    "X-Prometheus-Session-Id, X-Prometheus-Session-Key, Idempotency-Replayed, Retry-After";

/// The listener's browser and Host rules.
#[derive(Debug, Clone)]
pub(crate) struct Guard {
    /// The listener is bound to a loopback address.
    loopback: bool,
    public_host: Option<String>,
    origins: Vec<String>,
}

impl Guard {
    pub(crate) fn new(listener: &ApiListener, loopback: bool) -> Self {
        Self {
            loopback,
            public_host: listener
                .public_host
                .as_ref()
                .map(|h| h.to_ascii_lowercase()),
            origins: listener.cors_origins.clone(),
        }
    }

    /// The allowlisted origin of `req` (`None` without an `Origin`), or
    /// the 403 that refuses it.
    pub(crate) fn admit(&self, req: &Req) -> Result<Option<String>, Reply> {
        if self.loopback && !self.host_allowed(req.header("host")) {
            return Err(forbidden("the Host header names no loopback host"));
        }
        match req.header("origin") {
            None => Ok(None),
            Some(origin) if self.origins.iter().any(|o| o == origin) => Ok(Some(origin.to_owned())),
            Some(_) => Err(forbidden(
                "this origin is not allowed (api_server.cors_origins)",
            )),
        }
    }

    fn host_allowed(&self, host: Option<&str>) -> bool {
        let Some(host) = host else {
            return false;
        };
        let name = host_name(host).to_ascii_lowercase();
        matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1")
            || name.ends_with(".localhost")
            || name
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback())
            || self.public_host.as_deref() == Some(name.as_str())
    }

    /// The answer of a CORS preflight from allowlisted `origin`.
    pub(crate) fn preflight(origin: &str) -> Reply {
        let reply = hyper::Response::builder()
            .status(204)
            .body(full(bytes::Bytes::new()))
            .unwrap_or_else(|_| hyper::Response::new(full(bytes::Bytes::new())));
        let reply = with_header(
            reply,
            "access-control-allow-methods",
            "GET, POST, PATCH, DELETE, OPTIONS",
        );
        let reply = with_header(reply, "access-control-allow-headers", ALLOWED_HEADERS);
        let reply = with_header(reply, "access-control-max-age", "600");
        cors(reply, origin)
    }

    /// `reply` with the security headers, and the CORS headers for an
    /// allowlisted `origin`.
    pub(crate) fn finish(reply: Reply, origin: Option<&str>) -> Reply {
        let reply = with_header(reply, "x-content-type-options", "nosniff");
        let reply = with_header(reply, "referrer-policy", "no-referrer");
        match origin {
            Some(origin) => cors(reply, origin),
            None => reply,
        }
    }
}

fn cors(reply: Reply, origin: &str) -> Reply {
    let reply = with_header(reply, "access-control-allow-origin", origin);
    let reply = with_header(reply, "access-control-expose-headers", EXPOSED_HEADERS);
    with_header(reply, "vary", "Origin")
}

fn forbidden(message: &str) -> Reply {
    error(403, "permission_error", "forbidden", message)
}

/// The name part of a `Host` header (`[::1]:8642` is `::1`).
fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::{Guard, host_name};
    use crate::http::Req;
    use pmt_config::daemon_api::ApiListener;

    fn req(headers: &[(&str, &str)]) -> Req {
        let mut map = hyper::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        Req {
            method: hyper::Method::POST,
            path: "/v1/chat/completions".into(),
            query: None,
            headers: map,
            body: bytes::Bytes::new(),
        }
    }

    /// Scenario: Rebinding host refused.
    #[test]
    fn a_foreign_host_on_a_loopback_bind_is_refused() {
        let guard = Guard::new(&ApiListener::default(), true);
        let refused = guard
            .admit(&req(&[("host", "attacker.example:8642")]))
            .unwrap_err();
        assert_eq!(refused.status(), 403);
        assert!(guard.admit(&req(&[("host", "127.0.0.1:8642")])).is_ok());
        assert!(guard.admit(&req(&[("host", "localhost:8642")])).is_ok());
        assert!(guard.admit(&req(&[("host", "[::1]:8642")])).is_ok());
        let proxied = Guard::new(
            &ApiListener {
                public_host: Some("agent.example.com".into()),
                ..ApiListener::default()
            },
            true,
        );
        assert!(
            proxied
                .admit(&req(&[("host", "agent.example.com")]))
                .is_ok()
        );
    }

    /// Scenario: Web page on another origin.
    #[test]
    fn an_origin_outside_the_allowlist_is_refused() {
        let guard = Guard::new(&ApiListener::default(), true);
        let refused = guard
            .admit(&req(&[
                ("host", "127.0.0.1"),
                ("origin", "https://evil.example"),
            ]))
            .unwrap_err();
        assert_eq!(refused.status(), 403);
        let allowing = Guard::new(
            &ApiListener {
                cors_origins: vec!["http://localhost:3000".into()],
                ..ApiListener::default()
            },
            true,
        );
        let origin = allowing
            .admit(&req(&[
                ("host", "127.0.0.1"),
                ("origin", "http://localhost:3000"),
            ]))
            .unwrap();
        assert_eq!(origin.as_deref(), Some("http://localhost:3000"));
        let preflight = Guard::preflight("http://localhost:3000");
        assert_eq!(preflight.headers()["access-control-max-age"], "600");
        assert!(
            preflight.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .contains("Idempotency-Key")
        );
    }

    #[test]
    fn host_names_drop_their_port() {
        assert_eq!(host_name("example.com:80"), "example.com");
        assert_eq!(host_name("[::1]:8642"), "::1");
        assert_eq!(host_name("localhost"), "localhost");
    }
}
