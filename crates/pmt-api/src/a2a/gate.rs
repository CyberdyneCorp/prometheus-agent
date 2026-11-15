//! Who may talk to the A2A server (spec: agent-interop, A2A server
//! enablement and binding; A2A authentication, trust and rate limits; A2A
//! inbound safety).
//!
//! A profile without tokens answers only requests addressed to a loopback
//! name (or its `public_url` host) and refuses anything a browser sends: a
//! web page can neither rebind its name to 127.0.0.1 nor post a form or a
//! cross-origin fetch to the server. With tokens, the bearer names the
//! caller: a per-peer token its peer, the shared token the caller's IP.
//! Only trusted identities start turns; each identity is rate limited and
//! each conversation capped in exchanges per 10 minutes.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pmt_a2a::sign::constant_time_eq;
use pmt_config::schema::A2aServerConfig;
use pmt_core::secret::SecretString;

use crate::http::Req;

/// The identity of a caller on a profile without tokens.
pub(crate) const LOCAL: &str = "local";
/// The browser refusal.
pub(crate) const BROWSER_REFUSED: &str = "browser-originated request refused";
/// The window of the ping-pong cap.
const PINGPONG_WINDOW: Duration = Duration::from_mins(10);
/// The window of the rate limit.
const RATE_WINDOW: Duration = Duration::from_mins(1);

/// Why a request is refused before any method runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 403 with the message.
    Forbidden(&'static str),
    /// 401.
    Unauthorized,
}

/// The browser and Host rules of a profile without tokens.
pub(crate) fn admit_tokenless(req: &Req, public_host: Option<&str>) -> Result<(), Refusal> {
    let host = req.header("host").map(host_name).unwrap_or_default();
    let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback && public_host != Some(host.as_str()) {
        return Err(Refusal::Forbidden(
            "the Host header names neither a loopback host nor a2a.server.public_url",
        ));
    }
    if from_browser(req) {
        return Err(Refusal::Forbidden(BROWSER_REFUSED));
    }
    Ok(())
}

/// Whether a browser sent `req`: an `Origin`, a cross-site or same-site
/// fetch, or a POST whose body is not JSON (a form).
fn from_browser(req: &Req) -> bool {
    if req.header("origin").is_some() {
        return true;
    }
    if matches!(
        req.header("sec-fetch-site"),
        Some("cross-site" | "same-site")
    ) {
        return true;
    }
    req.method == hyper::Method::POST && !json_content(req.header("content-type"))
}

fn json_content(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let media = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media == "application/json" || media.ends_with("+json")
}

/// The lowercase host name of a `Host` header (no port, no brackets).
pub(crate) fn host_name(host: &str) -> String {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                name
            } else {
                host
            }
        })
    };
    name.to_ascii_lowercase()
}

/// The host of a URL (`https://agent.example.com/a2a` →
/// `agent.example.com`).
pub(crate) fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(host_name(authority))
}

/// The caller's identity from its bearer token.
pub(crate) fn identify(
    req: &Req,
    config: &A2aServerConfig,
    secret: &dyn Fn(&str) -> Option<SecretString>,
    caller: IpAddr,
) -> Result<(String, Option<String>), Refusal> {
    let given = req
        .header("authorization")
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::trim)
        .ok_or(Refusal::Unauthorized)?;
    // Every configured token is compared, so the time taken does not tell
    // which one matched.
    let mut found = None;
    for peer in &config.peers {
        if let Some(token) = secret(&peer.token_secret.0)
            && constant_time_eq(token.expose().as_bytes(), given.as_bytes())
            && found.is_none()
        {
            found = Some((peer.name.clone(), Some(token.expose().to_owned())));
        }
    }
    if let Some(name) = &config.shared_token_secret
        && let Some(token) = secret(&name.0)
        && constant_time_eq(token.expose().as_bytes(), given.as_bytes())
        && found.is_none()
    {
        found = Some((caller.to_string(), Some(token.expose().to_owned())));
    }
    found.ok_or(Refusal::Unauthorized)
}

/// Whether `identity` may start turns.
pub(crate) fn trusted(config: &A2aServerConfig, identity: &str) -> bool {
    config.allow_all_peers || config.trusted_peers.iter().any(|p| p == identity)
}

/// Requests per identity per minute.
#[derive(Default)]
pub(crate) struct RateLimits {
    seen: Mutex<HashMap<(String, String), Vec<Instant>>>,
}

impl RateLimits {
    /// Counts one request of `identity` on `profile`; `false` when it is
    /// over `per_minute`.
    pub(crate) fn admit(&self, profile: &str, identity: &str, per_minute: u32) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().expect("rate limits poisoned");
        seen.retain(|_, times| times.last().is_some_and(|t| now - *t < RATE_WINDOW));
        let times = seen
            .entry((profile.to_owned(), identity.to_owned()))
            .or_default();
        times.retain(|t| now - *t < RATE_WINDOW);
        if times.len() >= per_minute as usize {
            return false;
        }
        times.push(now);
        true
    }
}

/// Exchanges per conversation within 10 minutes.
#[derive(Default)]
pub(crate) struct PingPong {
    seen: Mutex<HashMap<String, Vec<Instant>>>,
}

impl PingPong {
    /// Counts one exchange of conversation `key`; `false` when `cap`
    /// exchanges already happened in the window.
    pub(crate) fn admit(&self, key: &str, cap: u32) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().expect("ping-pong poisoned");
        seen.retain(|_, times| times.last().is_some_and(|t| now - *t < PINGPONG_WINDOW));
        let times = seen.entry(key.to_owned()).or_default();
        times.retain(|t| now - *t < PINGPONG_WINDOW);
        if times.len() >= cap as usize {
            return false;
        }
        times.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(headers: &[(&str, &str)]) -> Req {
        let mut builder = hyper::Request::builder().method("POST").uri("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (parts, ()) = builder.body(()).unwrap().into_parts();
        Req::head(parts)
    }

    /// Scenario: Browser page refused.
    #[test]
    fn a_page_posting_with_an_origin_is_refused() {
        let page = req(&[
            ("host", "127.0.0.1:9900"),
            ("origin", "https://evil.example"),
            ("content-type", "application/json"),
        ]);
        assert_eq!(
            admit_tokenless(&page, None),
            Err(Refusal::Forbidden(BROWSER_REFUSED))
        );
        let form = req(&[("host", "127.0.0.1:9900"), ("content-type", "text/plain")]);
        assert_eq!(
            admit_tokenless(&form, None),
            Err(Refusal::Forbidden(BROWSER_REFUSED))
        );
        let site = req(&[
            ("host", "localhost:9900"),
            ("sec-fetch-site", "same-site"),
            ("content-type", "application/json"),
        ]);
        assert_eq!(
            admit_tokenless(&site, None),
            Err(Refusal::Forbidden(BROWSER_REFUSED))
        );
        let agent = req(&[
            ("host", "127.0.0.1:9900"),
            ("content-type", "application/json"),
        ]);
        assert_eq!(admit_tokenless(&agent, None), Ok(()));
    }

    #[test]
    fn a_rebound_name_is_refused_unless_it_is_the_public_host() {
        let rebound = req(&[
            ("host", "attacker.example:9900"),
            ("content-type", "application/json"),
        ]);
        assert!(admit_tokenless(&rebound, None).is_err());
        assert_eq!(admit_tokenless(&rebound, Some("attacker.example")), Ok(()));
        assert_eq!(
            url_host("https://Agent.Example.com:8443/a2a").as_deref(),
            Some("agent.example.com")
        );
        assert_eq!(host_name("[::1]:9900"), "::1");
    }

    /// Scenario: Ping-pong capped.
    #[test]
    fn the_sixth_exchange_in_a_context_is_capped() {
        let pingpong = PingPong::default();
        for _ in 0..5 {
            assert!(pingpong.admit("default:a2a:alice:c1", 5));
        }
        assert!(!pingpong.admit("default:a2a:alice:c1", 5));
        assert!(pingpong.admit("default:a2a:alice:c2", 5));
    }

    #[test]
    fn identities_are_limited_per_minute() {
        let limits = RateLimits::default();
        assert!(limits.admit("default", "alice", 2));
        assert!(limits.admit("default", "alice", 2));
        assert!(!limits.admit("default", "alice", 2));
        assert!(limits.admit("default", "bob", 2));
    }
}
