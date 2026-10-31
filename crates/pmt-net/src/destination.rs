//! Which addresses a model- or remote-supplied URL may reach (spec:
//! security-boundaries, Private and metadata destinations).
//!
//! Cloud metadata endpoints are refused unconditionally: they hand out the
//! host's cloud credentials. Other private addresses (loopback, link-local,
//! RFC 1918, ULA, CGNAT, benchmarking, IPv4-mapped) are refused unless
//! `security.egress.allow_private` is set or a session grant names them.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;
use pmt_core::error::{Error, codes};

/// Host names of metadata services (compared case-insensitively, without a
/// trailing dot).
const METADATA_HOSTS: &[&str] = &["metadata.google.internal"];
const METADATA_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);
/// The NAT64 well-known prefix `64:ff9b::/96`: a NAT64 gateway forwards
/// these to the embedded IPv4 address, so they are judged by it.
const NAT64_PREFIX: [u16; 6] = [0x0064, 0xff9b, 0, 0, 0, 0];

/// How an address is treated by the destination policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Reachable without a grant.
    Public,
    /// Needs `allow_private` or a session grant naming it.
    Private,
    /// A cloud metadata endpoint: always refused.
    Metadata,
}

/// Classifies `ip` (spec list, plus `0.0.0.0/8` and `::`, which reach the
/// local host on common stacks).
#[must_use]
pub fn classify(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> Class {
    if ip == METADATA_V4 {
        return Class::Metadata;
    }
    let [a, b, ..] = ip.octets();
    let private = ip.is_loopback()
        || ip.is_link_local()
        || ip.is_private()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19));
    if private {
        Class::Private
    } else {
        Class::Public
    }
}

fn classify_v6(ip: Ipv6Addr) -> Class {
    if ip == METADATA_V6 {
        return Class::Metadata;
    }
    if let Some(v4) = embedded_v4(ip) {
        // The whole mapped range is private; the embedded address can
        // still make it a metadata endpoint.
        return match classify_v4(v4) {
            Class::Metadata => Class::Metadata,
            Class::Private => Class::Private,
            Class::Public if ip.to_ipv4_mapped().is_some() => Class::Private,
            Class::Public => Class::Public,
        };
    }
    let first = ip.segments()[0];
    let private = ip.is_loopback()
        || ip.is_unspecified()
        || (first & 0xffc0) == 0xfe80
        || (first & 0xfe00) == 0xfc00;
    if private {
        Class::Private
    } else {
        Class::Public
    }
}

/// The IPv4 address carried by an IPv4-mapped or NAT64 address.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let seg = ip.segments();
    (seg[..6] == NAT64_PREFIX).then(|| {
        let [.., hi, lo] = seg;
        Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))
    })
}

/// Whether `host` is a metadata service name.
#[must_use]
pub fn is_metadata_host(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    METADATA_HOSTS.iter().any(|m| m.eq_ignore_ascii_case(host))
}

/// The host part of a `net.connect` grant that can name a private
/// destination.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    /// `*`: any host. It never names a private destination: those must be
    /// granted by name or address.
    Any,
    /// An exact host name (lower case).
    Name(String),
    /// `*.suffix`: any name under the suffix (lower case, leading dot).
    Suffix(String),
    /// An address or CIDR range.
    Net(IpNet),
}

/// A session grant for a private destination: the value of a
/// `net.connect:<host-pattern>[:<port>]` capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateGrant {
    host: HostPattern,
    port: Option<u16>,
}

impl PrivateGrant {
    /// Parses `<host-pattern>[:<port>]`: `nas.local`, `*.lan:8080`,
    /// `10.0.0.5`, `10.0.0.0/8:443`, `[fd00::1]:80`, `fd00::/8`.
    ///
    /// # Errors
    ///
    /// `net.invalid_url` naming the pattern when it cannot be parsed.
    pub fn parse(pattern: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::new(
                &codes::NET_INVALID_URL,
                format!("invalid net.connect destination '{pattern}'"),
            )
        };
        let (host, port) = split_port(pattern).ok_or_else(invalid)?;
        let host = parse_host_pattern(host).ok_or_else(invalid)?;
        Ok(Self { host, port })
    }

    fn names(&self, host: Option<&str>, ip: IpAddr, port: Option<u16>) -> bool {
        if let (Some(want), Some(port)) = (self.port, port)
            && want != port
        {
            return false;
        }
        match (&self.host, host) {
            (HostPattern::Net(net), _) => net.contains(&ip),
            (HostPattern::Name(name), Some(host)) => normalize(host) == *name,
            (HostPattern::Suffix(suffix), Some(host)) => normalize(host).ends_with(suffix.as_str()),
            (HostPattern::Any, _) | (_, None) => false,
        }
    }
}

fn normalize(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// Splits an optional `:<port>`; a bare IPv6 address has no port.
fn split_port(pattern: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = pattern.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return match tail.strip_prefix(':') {
            Some(port) => Some((host, Some(port.parse().ok()?))),
            None if tail.is_empty() => Some((host, None)),
            None => None,
        };
    }
    match pattern.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Some((host, Some(port.parse().ok()?))),
        _ => Some((pattern, None)),
    }
}

fn parse_host_pattern(host: &str) -> Option<HostPattern> {
    if host == "*" {
        return Some(HostPattern::Any);
    }
    if let Ok(net) = host.parse::<IpNet>() {
        return Some(HostPattern::Net(net));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(HostPattern::Net(IpNet::from(ip)));
    }
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    };
    match host.strip_prefix("*.") {
        Some(suffix) if valid(suffix) => {
            Some(HostPattern::Suffix(format!(".{}", normalize(suffix))))
        }
        None if valid(host) => Some(HostPattern::Name(normalize(host))),
        _ => None,
    }
}

/// Why a destination was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The host as written in the URL.
    pub host: String,
    /// The refused address, when the refusal is about one.
    pub ip: Option<IpAddr>,
    /// What made it refused.
    pub reason: RefusalReason,
}

/// The rule a refused destination broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// A cloud metadata endpoint (never allowed).
    Metadata,
    /// A private address without `allow_private` or a grant.
    Private,
    /// The host did not resolve (fail closed, no second resolver).
    Unresolved,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = &self.host;
        let at = self.ip.map(|ip| format!(" ({ip})")).unwrap_or_default();
        match self.reason {
            RefusalReason::Metadata => {
                write!(f, "destination {host}{at} is a cloud metadata endpoint")
            }
            RefusalReason::Private => write!(
                f,
                "destination {host}{at} is a private address; set security.egress.allow_private or grant net.connect:{host}"
            ),
            RefusalReason::Unresolved => write!(f, "destination {host} did not resolve"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        Error::new(&codes::NET_DESTINATION_BLOCKED, refusal.to_string())
    }
}

/// A session's destination policy for URL-taking tools and in-process
/// fetches.
#[derive(Debug, Clone, Default)]
pub struct DestinationPolicy {
    allow_private: bool,
    fake_ip_ranges: Vec<IpNet>,
    grants: Vec<PrivateGrant>,
}

impl DestinationPolicy {
    /// The policy from `security.egress.allow_private` and
    /// `security.egress.fake_ip_ranges`.
    ///
    /// # Errors
    ///
    /// `config.invalid` naming a range that is not CIDR notation.
    pub fn new(allow_private: bool, fake_ip_ranges: &[String]) -> Result<Self, Error> {
        let fake_ip_ranges = fake_ip_ranges
            .iter()
            .map(|range| {
                range.parse::<IpNet>().map_err(|_| {
                    Error::new(
                        &codes::CONFIG_INVALID,
                        format!("security.egress.fake_ip_ranges: '{range}' is not a CIDR range"),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            allow_private,
            fake_ip_ranges,
            grants: Vec::new(),
        })
    }

    /// Adds a session grant (`net.connect` value) that may name private
    /// destinations.
    #[must_use]
    pub fn with_grant(mut self, grant: PrivateGrant) -> Self {
        self.grants.push(grant);
        self
    }

    /// Refuses metadata host names before any resolution.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] for a metadata service name.
    pub fn check_host(&self, host: &str) -> Result<(), Refusal> {
        if is_metadata_host(host) {
            return Err(Refusal {
                host: host.to_owned(),
                ip: None,
                reason: RefusalReason::Metadata,
            });
        }
        Ok(())
    }

    /// Checks one address `host` resolved to (`host` = `None` for an IP
    /// literal); `port` = `None` skips the port of grants (the caller
    /// checked it already).
    ///
    /// # Errors
    ///
    /// The [`Refusal`] when the address may not be reached.
    pub fn check_addr(
        &self,
        host: Option<&str>,
        ip: IpAddr,
        port: Option<u16>,
    ) -> Result<(), Refusal> {
        let refuse = |reason| Refusal {
            host: host.map_or_else(|| ip.to_string(), str::to_owned),
            ip: Some(ip),
            reason,
        };
        if host.is_some_and(is_metadata_host) {
            return Err(refuse(RefusalReason::Metadata));
        }
        match classify(ip) {
            Class::Public => Ok(()),
            Class::Metadata => Err(refuse(RefusalReason::Metadata)),
            Class::Private if self.private_allowed(host, ip, port) => Ok(()),
            Class::Private => Err(refuse(RefusalReason::Private)),
        }
    }

    fn private_allowed(&self, host: Option<&str>, ip: IpAddr, port: Option<u16>) -> bool {
        // Fake-IP answers of a local TUN proxy stand for public hosts.
        self.allow_private
            || self.fake_ip_ranges.iter().any(|net| net.contains(&ip))
            || self.grants.iter().any(|g| g.names(host, ip, port))
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{Class, DestinationPolicy, PrivateGrant, RefusalReason, classify};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn spec_private_ranges_are_private() {
        for addr in [
            "127.0.0.1",
            "::1",
            "169.254.1.1",
            "fe80::1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "fd12::1",
            "fc00::1",
            "::ffff:8.8.8.8",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "0.0.0.0",
            "::",
            "64:ff9b::10.0.0.1",
        ] {
            assert_eq!(classify(ip(addr)), Class::Private, "{addr}");
        }
    }

    #[test]
    fn public_addresses_stay_public() {
        for addr in [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "198.20.0.1",
            "2606:4700::1",
            "64:ff9b::8.8.8.8",
        ] {
            assert_eq!(classify(ip(addr)), Class::Public, "{addr}");
        }
    }

    #[test]
    fn metadata_endpoints_and_their_mapped_forms() {
        for addr in [
            "169.254.169.254",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "64:ff9b::169.254.169.254",
        ] {
            assert_eq!(classify(ip(addr)), Class::Metadata, "{addr}");
        }
    }

    #[test]
    fn metadata_is_refused_even_when_private_is_allowed_or_granted() {
        let policy = DestinationPolicy::new(true, &["169.254.0.0/16".into()])
            .unwrap()
            .with_grant(PrivateGrant::parse("169.254.169.254").unwrap());
        let refusal = policy
            .check_addr(None, ip("::ffff:169.254.169.254"), Some(80))
            .unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::Metadata);
        assert!(policy.check_host("Metadata.Google.Internal.").is_err());
        assert!(
            policy
                .check_addr(Some("metadata.google.internal"), ip("8.8.8.8"), None)
                .is_err()
        );
    }

    #[test]
    fn private_needs_allow_private_or_a_naming_grant() {
        let deny = DestinationPolicy::new(false, &[]).unwrap();
        let err = deny
            .check_addr(Some("nas.lan"), ip("192.168.1.5"), Some(80))
            .unwrap_err();
        assert_eq!(err.reason, RefusalReason::Private);
        assert!(err.to_string().contains("net.connect:nas.lan"));
        assert!(
            DestinationPolicy::new(true, &[])
                .unwrap()
                .check_addr(None, ip("10.0.0.1"), None)
                .is_ok()
        );

        let granted = DestinationPolicy::new(false, &[])
            .unwrap()
            .with_grant(PrivateGrant::parse("*.lan:8080").unwrap())
            .with_grant(PrivateGrant::parse("[fd00::1]").unwrap())
            .with_grant(PrivateGrant::parse("*").unwrap());
        assert!(
            granted
                .check_addr(Some("nas.LAN"), ip("192.168.1.5"), Some(8080))
                .is_ok()
        );
        assert!(
            granted
                .check_addr(Some("nas.lan"), ip("192.168.1.5"), Some(22))
                .is_err()
        );
        assert!(granted.check_addr(None, ip("fd00::1"), Some(1)).is_ok());
        assert!(
            granted
                .check_addr(Some("other"), ip("10.0.0.1"), Some(80))
                .is_err(),
            "`*` does not name private destinations"
        );
    }

    #[test]
    fn fake_ip_ranges_count_as_public() {
        let policy = DestinationPolicy::new(false, &["198.18.0.0/15".into()]).unwrap();
        assert!(
            policy
                .check_addr(Some("x.com"), ip("198.18.3.4"), Some(443))
                .is_ok()
        );
        assert!(DestinationPolicy::new(false, &["nope".into()]).is_err());
    }

    #[test]
    fn grant_patterns_parse() {
        for ok in [
            "nas.local",
            "*.lan:8080",
            "10.0.0.0/8:443",
            "[fd00::1]:80",
            "fd00::/8",
            "*",
        ] {
            assert!(PrivateGrant::parse(ok).is_ok(), "{ok}");
        }
        for bad in ["", "a b", "host:port", "[fd00::1", "*.:1"] {
            assert!(PrivateGrant::parse(bad).is_err(), "{bad}");
        }
    }
}
