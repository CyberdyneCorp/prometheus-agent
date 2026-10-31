//! The `network` doctor group's IPv6 check. Without `--online` it only
//! reports whether a global IPv6 route exists, which sends no packet. With
//! `--online` and a route, each target host gets a 2 s IPv6-only and a 2 s
//! IPv4-only TCP probe; IPv6 failing while IPv4 works is the blackhole that
//! `network.force_ipv4` avoids.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use pmt_core::doctor::{Check, Status};
use tokio::net::TcpStream;

use crate::resolve::Lookup;

/// The doctor group name.
pub const GROUP: &str = "network";

/// Per-family probe budget.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The warning for a blackholed IPv6 route (spec text).
pub const BLACKHOLE_WARNING: &str =
    "IPv6 route present but unreachable (blackhole); set network.force_ipv4 = true";

/// A documentation-range address: connecting a UDP socket to it only asks
/// the kernel for a route and a source address, and sends nothing.
const ROUTE_PROBE: &str = "[2001:db8::1]:53";

/// Whether the host has a route to the global IPv6 internet, judged by the
/// source address the kernel would pick (global unicast `2000::/3`).
#[must_use]
pub fn has_global_ipv6_route() -> bool {
    let Ok(socket) = UdpSocket::bind("[::]:0") else {
        return false;
    };
    socket.connect(ROUTE_PROBE).is_ok()
        && socket
            .local_addr()
            .is_ok_and(|addr| is_global_unicast(addr.ip()))
}

fn is_global_unicast(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xe000 == 0x2000)
}

/// One host to probe, usually a configured provider's API host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host name.
    pub host: String,
    /// TCP port (443 for HTTPS endpoints).
    pub port: u16,
}

/// Inputs of the IPv6 check.
#[derive(Debug, Clone)]
pub struct Ipv6Check {
    /// Result of [`has_global_ipv6_route`].
    pub route: bool,
    /// `--online`: probing is allowed.
    pub online: bool,
    /// The profile's `network.force_ipv4`.
    pub force_ipv4: bool,
    /// Hosts to probe with `--online`.
    pub targets: Vec<Target>,
    /// Per-family probe budget ([`PROBE_TIMEOUT`] in production).
    pub timeout: Duration,
}

impl Ipv6Check {
    /// Runs the check: one route line, plus one line per target when
    /// online and a route exists.
    pub async fn run(&self, lookup: &dyn Lookup) -> Vec<Check> {
        let mut checks = vec![self.route_check()];
        if self.route && self.online {
            for target in &self.targets {
                checks.push(self.probe(lookup, target).await);
            }
        }
        checks
    }

    fn route_check(&self) -> Check {
        let message = if self.route {
            "global IPv6 route present"
        } else {
            "no global IPv6 route; outbound connections use IPv4"
        };
        Check::new(GROUP, "ipv6_route", Status::Ok, message)
    }

    async fn probe(&self, lookup: &dyn Lookup, target: &Target) -> Check {
        let name = &target.host;
        let addrs = match lookup.lookup(name).await {
            Ok(addrs) => addrs,
            Err(e) => {
                return reachability(Status::Fail, format!("{name}: DNS lookup failed: {e}"))
                    .with_hint("check DNS settings or the configured host");
            }
        };
        let (v6, v4): (Vec<IpAddr>, Vec<IpAddr>) = addrs.into_iter().partition(IpAddr::is_ipv6);
        if v6.is_empty() {
            return reachability(Status::Ok, format!("{name} has no IPv6 address"));
        }
        if self.connects(&v6, target.port).await {
            return reachability(Status::Ok, format!("{name} is reachable over IPv6"));
        }
        if !self.connects(&v4, target.port).await {
            return reachability(
                Status::Fail,
                format!("{name}:{} is unreachable over IPv4 and IPv6", target.port),
            )
            .with_hint("check the network connection, proxy or firewall");
        }
        if self.force_ipv4 {
            return reachability(
                Status::Ok,
                format!(
                    "IPv6 to {name} is blackholed; network.force_ipv4 is set, so outbound uses IPv4"
                ),
            );
        }
        reachability(Status::Warn, BLACKHOLE_WARNING).with_hint(format!(
            "{name} answered over IPv4 only; run: prometheus config set network.force_ipv4 true"
        ))
    }

    /// Whether any address accepts a TCP connection within the budget.
    async fn connects(&self, addrs: &[IpAddr], port: u16) -> bool {
        let attempt = async {
            for ip in addrs {
                if TcpStream::connect(SocketAddr::new(*ip, port)).await.is_ok() {
                    return true;
                }
            }
            false
        };
        tokio::time::timeout(self.timeout, attempt)
            .await
            .unwrap_or(false)
    }
}

fn reachability(status: Status, message: impl Into<String>) -> Check {
    Check::new(GROUP, "ipv6_reachability", status, message)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::is_global_unicast;

    #[test]
    fn only_2000_slash_3_counts_as_global() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_global_unicast(ip("2a01:4f8::1")));
        assert!(!is_global_unicast(ip("fe80::1")));
        assert!(!is_global_unicast(ip("fd00::1")));
        assert!(!is_global_unicast(ip("::1")));
        assert!(!is_global_unicast(ip("192.0.2.1")));
    }

    #[test]
    fn route_detection_never_panics() {
        // The answer depends on the host; the call must be safe offline.
        let _ = super::has_global_ipv6_route();
    }
}
