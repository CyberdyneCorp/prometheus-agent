//! Local endpoint validation (spec: desktop-app, Local endpoint
//! validation). The app-managed `local` connection accepts only what a
//! daemon on this machine reports: plain loopback with an explicit port and
//! the protocol's RPC path, a daemon instance, a protocol major this app
//! speaks and session attachment. Anything else is refused; non-loopback
//! daemons are only reachable as registered remote or SSH connections.

use std::net::IpAddr;

use url::{Host, Url};

use crate::ensure::EnsureReport;

/// The protocol feature the desktop needs from the daemon.
pub const SESSION_ATTACH: &str = "session-attach";
/// The daemon's WebSocket path.
pub const RPC_PATH: &str = "/rpc";

/// A validated local daemon endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    /// The loopback WebSocket URL.
    pub rpc_url: String,
    /// The control channel.
    pub control: String,
    /// Random per daemon start.
    pub instance_id: String,
    /// The daemon's build id.
    pub build_id: String,
    /// The daemon's semantic version.
    pub version: String,
    /// The protocol major both sides speak.
    pub protocol_major: u16,
}

/// `Invalid local gateway endpoint: <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid local gateway endpoint: {}", self.0)
    }
}

impl std::error::Error for Invalid {}

/// Checks an ensure report for the `local` connection of an app that
/// speaks protocol major `major`.
///
/// # Errors
///
/// [`Invalid`] naming the first problem found.
pub fn validate_local(report: &EnsureReport, major: u16) -> Result<LocalEndpoint, Invalid> {
    let invalid = |reason: &str| Invalid(reason.to_owned());
    let url = Url::parse(&report.rpc_url).map_err(|_| invalid("not a URL"))?;
    // `ws` is the WebSocket form of an `http` origin; TLS is never needed
    // (and never expected) on loopback.
    if !matches!(url.scheme(), "ws" | "http") {
        return Err(invalid("not http"));
    }
    if !is_loopback(url.host().as_ref()) {
        return Err(invalid("not loopback"));
    }
    if url.port().is_none() {
        return Err(invalid("no explicit port"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("credentials in the URL"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("query or fragment in the URL"));
    }
    if url.path() != RPC_PATH {
        return Err(invalid("unexpected path"));
    }
    if report.instance_id.trim().is_empty() {
        return Err(invalid("no instance_id"));
    }
    let protocol_major = report
        .rpc_protocol
        .split_once('.')
        .and_then(|(m, _)| m.parse::<u16>().ok())
        .ok_or_else(|| invalid("unreadable rpc_protocol"))?;
    if protocol_major != major {
        return Err(Invalid(format!(
            "protocol {} is not supported (this app speaks {major}.x)",
            report.rpc_protocol
        )));
    }
    if !report.features.iter().any(|f| f == SESSION_ATTACH) {
        return Err(invalid("no session-attach feature"));
    }
    Ok(LocalEndpoint {
        rpc_url: report.rpc_url.clone(),
        control: report.control.clone(),
        instance_id: report.instance_id.clone(),
        build_id: report.build_id.clone(),
        version: report.version.clone(),
        protocol_major,
    })
}

/// Exactly `127.0.0.1` or `[::1]`: not `localhost`, which a hosts file
/// can point anywhere.
fn is_loopback(host: Option<&Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ip)) => IpAddr::V4(*ip) == IpAddr::from([127, 0, 0, 1]),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{EnsureReport, validate_local};

    fn report(rpc_url: &str) -> EnsureReport {
        EnsureReport {
            rpc_url: rpc_url.into(),
            control: "/run/c.sock".into(),
            instance_id: "abc".into(),
            build_id: "b".into(),
            version: "1.0.0".into(),
            rpc_protocol: "1.0".into(),
            features: vec!["session-attach".into()],
        }
    }

    fn reason(r: &EnsureReport) -> String {
        validate_local(r, 1).unwrap_err().to_string()
    }

    #[test]
    fn a_non_loopback_endpoint_is_refused() {
        assert_eq!(
            reason(&report("http://192.168.1.5:9120")),
            "Invalid local gateway endpoint: not loopback"
        );
        assert_eq!(
            reason(&report("ws://localhost:9120/rpc")),
            "Invalid local gateway endpoint: not loopback"
        );
    }

    #[test]
    fn only_a_plain_loopback_origin_with_the_rpc_path_is_accepted() {
        assert!(validate_local(&report("ws://127.0.0.1:4000/rpc"), 1).is_ok());
        assert!(validate_local(&report("ws://[::1]:4000/rpc"), 1).is_ok());
        for (url, why) in [
            ("wss://127.0.0.1:4000/rpc", "not http"),
            ("ws://127.0.0.1/rpc", "no explicit port"),
            ("ws://u:p@127.0.0.1:4000/rpc", "credentials in the URL"),
            (
                "ws://127.0.0.1:4000/rpc?x=1",
                "query or fragment in the URL",
            ),
            ("ws://127.0.0.1:4000/rpc#f", "query or fragment in the URL"),
            ("ws://127.0.0.1:4000/other", "unexpected path"),
        ] {
            assert_eq!(
                reason(&report(url)),
                format!("Invalid local gateway endpoint: {why}")
            );
        }
    }

    #[test]
    fn instance_protocol_and_session_attach_are_required() {
        let mut r = report("ws://127.0.0.1:4000/rpc");
        r.instance_id = String::new();
        assert!(reason(&r).ends_with("no instance_id"));
        let mut r = report("ws://127.0.0.1:4000/rpc");
        r.rpc_protocol = "2.3".into();
        assert!(reason(&r).ends_with("protocol 2.3 is not supported (this app speaks 1.x)"));
        let mut r = report("ws://127.0.0.1:4000/rpc");
        r.features.clear();
        assert!(reason(&r).ends_with("no session-attach feature"));
    }
}
