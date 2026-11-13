//! What every request passes before routing (spec: web-dashboard, Host
//! header validation; Same-origin only; Reverse-proxy base path from
//! config): the `Host` check against DNS rebinding, the client address
//! (forwarded only from trusted proxies), the dashboard's own origin, and
//! the security headers set on every answer. No CORS header is ever sent.

use std::net::{IpAddr, SocketAddr};

use hyper::header::HeaderValue;
use pmt_config::daemon_dashboard::{DashboardConfig, WebappConfig};

use crate::http::{Reply, Req};

/// `Content-Security-Policy` of every answer that sets none of its own.
/// Images may also be `data:` URIs (pairing QR codes the daemon draws).
pub(crate) const CSP: &str = "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'";
/// The SPA shell's policy when the webapp is on: the workspace previews
/// any http(s) page, always in a sandboxed frame (no popups, no
/// top-level navigation, an opaque origin).
pub(crate) const CSP_WEBAPP: &str = "default-src 'self'; img-src 'self' data:; frame-src 'self' http: https:; frame-ancestors 'none'";

/// The listener's view of who may address it and how.
#[derive(Debug, Clone)]
pub(crate) struct Guard {
    /// The bound port (Host headers of loopback names must carry it).
    port: u16,
    /// The configured bind host (accepted as is).
    bound_host: String,
    public_host: Option<String>,
    /// Hosts of `dashboard.public_url` and `webapp.public_url`.
    public_urls: Vec<String>,
    /// The listener serves TLS, or a trusted proxy terminates it.
    https: bool,
    trust_proxy: bool,
    trusted_proxies: Vec<IpAddr>,
}

/// How a `Host` header names the listener.
fn split_host(host: &str) -> Option<(String, Option<u16>)> {
    if host.is_empty()
        || host
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '@')
    {
        return None;
    }
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        (format!("[{inner}]"), port)
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) => (name.to_owned(), Some(port.parse::<u16>().ok()?)),
            None => (host.to_owned(), None),
        }
    };
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
    valid.then(|| (name.to_ascii_lowercase(), port))
}

impl Guard {
    pub(crate) fn new(config: &DashboardConfig, webapp: &WebappConfig, port: u16) -> Self {
        let public_urls = [
            config.public_url.as_deref(),
            Some(webapp.public_url.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter_map(|url| url.split_once("://").map(|(_, rest)| rest))
        .map(|rest| {
            rest.split('/')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase()
        })
        .filter(|host| !host.is_empty())
        .collect();
        Self {
            port,
            bound_host: config.host.to_ascii_lowercase(),
            public_host: config.public_host.as_ref().map(|h| h.to_ascii_lowercase()),
            public_urls,
            https: config.has_tls(),
            trust_proxy: config.trust_proxy,
            trusted_proxies: config
                .trusted_proxies
                .iter()
                .filter_map(|p| p.parse().ok())
                .collect(),
        }
    }

    /// Whether `host` (a `Host` header) names this listener: a loopback
    /// name with the bound port, the exact bound host, or
    /// `dashboard.public_host`.
    pub(crate) fn host_allowed(&self, host: &str) -> bool {
        let Some((name, port)) = split_host(host) else {
            return false;
        };
        let loopback = matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]");
        if loopback && port == Some(self.port) {
            return true;
        }
        let bound = self
            .bound_host
            .trim_start_matches('[')
            .trim_end_matches(']');
        let bare = name.trim_start_matches('[').trim_end_matches(']');
        if bare == bound && port.is_none_or(|p| p == self.port) {
            return true;
        }
        if self
            .public_urls
            .iter()
            .any(|h| *h == host.to_ascii_lowercase())
        {
            return true;
        }
        self.public_host.as_deref().is_some_and(|public| {
            split_host(public).is_some_and(|(p_name, p_port)| {
                p_name == name && (p_port.is_none() || p_port == port)
            })
        })
    }

    /// The request's `Host` header when it names this listener.
    pub(crate) fn host_of<'a>(&self, req: &'a Req) -> Option<&'a str> {
        req.header("host").filter(|h| self.host_allowed(h))
    }

    /// The scheme browsers use to reach the dashboard.
    pub(crate) fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    /// The dashboard's own origin as the browser sees it (from the
    /// already-validated `Host`).
    pub(crate) fn origin(&self, host: &str) -> String {
        format!("{}://{host}", self.scheme())
    }

    /// The origin the browser used: a trusted proxy's
    /// `X-Forwarded-Proto` and `X-Forwarded-Host` (the host still has to
    /// name this listener), else the scheme and validated `Host`.
    pub(crate) fn browser_origin(&self, req: &Req, peer: SocketAddr) -> Option<String> {
        let trusted = self.trust_proxy && self.trusted_proxies.contains(&peer.ip());
        let forwarded = |name: &str| {
            trusted
                .then(|| req.header(name))
                .flatten()
                .and_then(|v| v.split(',').next())
                .map(str::trim)
        };
        let host = match forwarded("x-forwarded-host") {
            Some(host) if self.host_allowed(host) => host,
            Some(_) => return None,
            None => self.host_of(req)?,
        };
        let scheme = match forwarded("x-forwarded-proto") {
            Some(proto @ ("http" | "https")) => proto,
            _ => self.scheme(),
        };
        Some(format!("{scheme}://{host}"))
    }

    /// The client's address: the peer, or the first `X-Forwarded-For`
    /// entry when the peer is a trusted proxy and `trust_proxy` is on.
    pub(crate) fn client_ip(&self, req: &Req, peer: SocketAddr) -> IpAddr {
        let trusted = self.trust_proxy && self.trusted_proxies.contains(&peer.ip());
        if !trusted {
            return peer.ip();
        }
        req.header("x-forwarded-for")
            .and_then(|list| list.split(',').next())
            .and_then(|first| first.trim().parse().ok())
            .unwrap_or_else(|| peer.ip())
    }

    /// Cookies are `Secure` when browsers reach the dashboard over TLS.
    pub(crate) fn secure_cookies(&self) -> bool {
        self.https
    }
}

/// Sets the security headers on `reply`; a reply that set its own policy
/// or framing rule (plugin frames, the webapp shell) keeps it.
pub(crate) fn finish(mut reply: Reply) -> Reply {
    let headers = reply.headers_mut();
    for (name, value) in [
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
    ] {
        headers
            .entry(name)
            .or_insert_with(|| HeaderValue::from_static(value));
    }
    reply
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_dashboard::DashboardConfig;

    use super::Guard;

    fn guard() -> Guard {
        let config = DashboardConfig {
            public_host: Some("dash.example.com".into()),
            ..DashboardConfig::default()
        };
        Guard::new(
            &config,
            &pmt_config::daemon_dashboard::WebappConfig::default(),
            9120,
        )
    }

    /// Scenario: DNS rebinding.
    #[test]
    fn only_the_listeners_own_names_pass() {
        let guard = guard();
        for host in [
            "127.0.0.1:9120",
            "localhost:9120",
            "LOCALHOST:9120",
            "[::1]:9120",
            "127.0.0.1",
            "dash.example.com",
            "dash.example.com:443",
        ] {
            assert!(guard.host_allowed(host), "{host}");
        }
        for host in [
            "evil.example:9120",
            "localhost:9121",
            "localhost",
            "127.0.0.1:x",
            "",
            "a b",
            "user@127.0.0.1:9120",
            "[::1",
        ] {
            assert!(!guard.host_allowed(host), "{host}");
        }
    }
}
