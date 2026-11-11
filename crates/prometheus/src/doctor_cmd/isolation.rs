//! Container and network-isolation checks (spec: distribution-and-update,
//! Container audio bridge and host services; Network-isolated deployment
//! recipe):
//!
//! - in a container, a provider whose base URL is `localhost` reaches the
//!   container's own loopback, not the host: `doctor providers` warns and
//!   suggests `host.docker.internal`
//! - `doctor network --online` reports whether a direct connection leaves
//!   the host (`direct egress: blocked` behind `compose.isolated.yaml`)
//!   and, through `network.proxy`, whether each provider host is
//!   reachable; with `network.expect_isolated = true` open direct egress
//!   fails the check
//! - `doctor network --allowlist` prints the hosts the egress proxy must
//!   allow for the profiles' configuration

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::time::Duration;

use pmt_core::doctor::{Check, Status};

/// Addresses a direct connection is tried to (by IP: an isolated network
/// may resolve names and still block traffic).
pub(super) const EGRESS_PROBES: [&str; 2] = ["1.1.1.1:443", "8.8.8.8:443"];

const TIMEOUT: Duration = Duration::from_secs(3);

/// The loopback warning for each `(provider, base URL)` when `in_container`.
pub(super) fn loopback_checks(endpoints: &[(String, String)], in_container: bool) -> Vec<Check> {
    if !in_container {
        return Vec::new();
    }
    endpoints
        .iter()
        .filter_map(|(provider, url)| {
            let parsed = reqwest::Url::parse(url).ok()?;
            let host = parsed.host_str()?;
            if !matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1") {
                return None;
            }
            let port = parsed.port_or_known_default()?;
            let mut suggested = parsed.clone();
            suggested.set_host(Some("host.docker.internal")).ok()?;
            Some(
                Check::new(
                    "providers",
                    "container-loopback",
                    Status::Warn,
                    format!(
                        "{provider} points at the container's own loopback; use http://host.docker.internal:{port} to reach a server on the host"
                    ),
                )
                .with_hint(format!(
                    "set its base URL to {}",
                    suggested.as_str().trim_end_matches('/')
                )),
            )
        })
        .collect()
}

/// Whether any of `probes` accepts a direct TCP connection.
pub(super) fn direct_egress(probes: &[&str]) -> bool {
    probes
        .iter()
        .filter_map(|p| p.parse::<SocketAddr>().ok())
        .any(|addr| TcpStream::connect_timeout(&addr, TIMEOUT).is_ok())
}

/// The direct-egress check.
pub(super) fn egress_check(open: bool, expect_isolated: bool) -> Check {
    match (open, expect_isolated) {
        (true, true) => Check::new(
            "network",
            "isolation",
            Status::Fail,
            "direct egress succeeded although network.expect_isolated = true",
        )
        .with_hint("attach the container only to the internal network of compose.isolated.yaml"),
        (true, false) => Check::new("network", "isolation", Status::Ok, "direct egress: open"),
        (false, _) => Check::new("network", "isolation", Status::Ok, "direct egress: blocked"),
    }
}

/// Whether `proxy` (an `http://host:port` URL) tunnels to `host:port`.
pub(super) fn through_proxy(proxy: &str, host: &str, port: u16) -> Result<(), String> {
    let url = reqwest::Url::parse(proxy).map_err(|e| format!("network.proxy: {e}"))?;
    let (Some(proxy_host), Some(proxy_port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err("network.proxy names no host".to_owned());
    };
    let addr = (proxy_host, proxy_port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve the proxy: {e}"))?
        .next()
        .ok_or("cannot resolve the proxy")?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)
        .map_err(|e| format!("proxy unreachable: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("proxy write failed: {e}"))?;
    let mut head = [0u8; 64];
    let read = stream
        .read(&mut head)
        .map_err(|e| format!("no answer from the proxy: {e}"))?;
    let line = String::from_utf8_lossy(&head[..read]);
    let status = line.split_whitespace().nth(1).unwrap_or("?").to_owned();
    if status == "200" {
        Ok(())
    } else {
        Err(format!("the proxy answered {status}"))
    }
}

/// One check per provider host through the proxy.
pub(super) fn proxy_checks(proxy: Option<&str>, hosts: &[(String, String, u16)]) -> Vec<Check> {
    let Some(proxy) = proxy else {
        return vec![
            Check::new(
                "network",
                "proxy",
                Status::Warn,
                "network.proxy is not set: an isolated deployment has no way out",
            )
            .with_hint("set network.proxy to the egress proxy, e.g. http://egress-proxy:3128"),
        ];
    };
    hosts
        .iter()
        .map(
            |(provider, host, port)| match through_proxy(proxy, host, *port) {
                Ok(()) => Check::new(
                    "network",
                    "proxy",
                    Status::Ok,
                    format!("{provider}: {host}:{port} reachable through the proxy"),
                ),
                Err(why) => Check::new(
                    "network",
                    "proxy",
                    Status::Fail,
                    format!("{provider}: {host}:{port} not reachable through the proxy ({why})"),
                )
                .with_hint(
                    "add the host to the proxy allowlist (prometheus doctor network --allowlist)",
                ),
            },
        )
        .collect()
}

/// Hosts the release origin downloads from (GitHub release assets).
const RELEASE_HOSTS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The hosts an egress proxy must allow: every profile's providers and
/// HTTP MCP servers, the release origin (or `update.mirror_url`) and the
/// runtime archives this build pins (or `runtimes.mirrors.runtimes`).
pub(super) fn allowlist(
    scopes: &[pmt_config::ProfileScope],
    daemon: &pmt_config::DaemonConfig,
) -> Vec<String> {
    let host_of = |url: &str| reqwest::Url::parse(url).ok()?.host_str().map(str::to_owned);
    let mut hosts: Vec<String> = Vec::new();
    for scope in scopes {
        hosts.extend(
            super::system::provider_endpoints(scope)
                .into_iter()
                .map(|(_, host, _)| host),
        );
        hosts.extend(
            scope
                .config()
                .mcp
                .servers
                .values()
                .filter_map(|server| host_of(&server.url)),
        );
    }
    match &daemon.update.mirror_url {
        Some(mirror) => hosts.extend(host_of(mirror)),
        None => hosts.extend(RELEASE_HOSTS.iter().map(|h| (*h).to_owned())),
    }
    if let Some(mirror) = &daemon.runtimes.mirrors.runtimes {
        hosts.extend(host_of(mirror));
    } else {
        let index = pmt_update::runtimes::index::RuntimeIndex::embedded();
        hosts.extend(
            index
                .runtimes
                .values()
                .flat_map(|pin| pin.artifacts.values().filter_map(|a| host_of(&a.url))),
        );
    }
    hosts.retain(|h| !matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1"));
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use pmt_core::doctor::Status;

    use super::{egress_check, loopback_checks};

    #[test]
    fn a_local_provider_in_a_container_is_pointed_at_the_host() {
        let endpoints = vec![
            ("ollama".to_owned(), "http://localhost:11434/v1".to_owned()),
            ("cloud".to_owned(), "https://api.example.com/v1".to_owned()),
        ];
        assert!(loopback_checks(&endpoints, false).is_empty());
        let checks = loopback_checks(&endpoints, true);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Warn);
        assert_eq!(
            checks[0].message,
            "ollama points at the container's own loopback; use http://host.docker.internal:11434 to reach a server on the host"
        );
        assert_eq!(
            checks[0].hint.as_deref(),
            Some("set its base URL to http://host.docker.internal:11434/v1")
        );
    }

    #[test]
    fn open_egress_fails_only_when_isolation_is_expected() {
        assert_eq!(egress_check(true, true).status, Status::Fail);
        assert_eq!(
            egress_check(true, true).message,
            "direct egress succeeded although network.expect_isolated = true"
        );
        assert_eq!(egress_check(true, false).status, Status::Ok);
        assert_eq!(egress_check(false, true).message, "direct egress: blocked");
    }
}
