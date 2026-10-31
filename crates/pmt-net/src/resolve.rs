//! Name resolution for outbound clients. With `network.force_ipv4` only A
//! records are returned, so no IPv6 connection is ever attempted.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use crate::destination::{DestinationPolicy, Refusal, RefusalReason};

/// A boxed, sendable lookup future.
pub type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Turns a host name into addresses. The system resolver is the only
/// production implementation; tests substitute fixed answers.
pub trait Lookup: Send + Sync {
    /// All addresses of `host`, in resolver order.
    fn lookup(&self, host: &str) -> LookupFuture;
}

/// The operating system resolver (`getaddrinfo`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: &str) -> LookupFuture {
        let host = host.to_owned();
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    }
}

/// Keeps only IPv4 addresses when `force_ipv4` is set.
#[must_use]
pub fn filter(addrs: Vec<IpAddr>, force_ipv4: bool) -> Vec<IpAddr> {
    if force_ipv4 {
        addrs.into_iter().filter(IpAddr::is_ipv4).collect()
    } else {
        addrs
    }
}

/// The resolver plugged into the shared HTTP client. With a destination
/// policy (the SSRF-safe [`crate::fetch::Fetcher`]) every resolved address
/// is checked and only checked addresses are handed to the connector, so a
/// rebinding answer between the pre-flight check and the connect is caught.
#[derive(Clone)]
pub(crate) struct OutboundResolver {
    pub(crate) lookup: Arc<dyn Lookup>,
    pub(crate) force_ipv4: bool,
    pub(crate) destinations: Option<Arc<DestinationPolicy>>,
}

impl OutboundResolver {
    /// The checked addresses of `host`.
    pub(crate) async fn resolve_checked(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let found = self.lookup.lookup(host).await;
        let Some(policy) = &self.destinations else {
            return non_empty(host, filter(found?, self.force_ipv4), self.force_ipv4);
        };
        let unresolved = || Refusal {
            host: host.to_owned(),
            ip: None,
            reason: RefusalReason::Unresolved,
        };
        // Fail closed: a resolver error is a refusal, never a retry elsewhere.
        let addrs = filter(found.map_err(|_| refused(unresolved()))?, self.force_ipv4);
        if addrs.is_empty() {
            return Err(refused(unresolved()));
        }
        for ip in &addrs {
            policy.check_addr(Some(host), *ip, None).map_err(refused)?;
        }
        Ok(addrs)
    }
}

/// Wraps a refusal so it can be recognised in a transport error chain.
pub(crate) fn refused(refusal: Refusal) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, refusal)
}

fn non_empty(host: &str, addrs: Vec<IpAddr>, force_ipv4: bool) -> io::Result<Vec<IpAddr>> {
    if addrs.is_empty() {
        let why = if force_ipv4 {
            "no IPv4 address (network.force_ipv4 is set)"
        } else {
            "no address"
        };
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host}: {why}"),
        ));
    }
    Ok(addrs)
}

impl reqwest::dns::Resolve for OutboundResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.clone();
        Box::pin(async move {
            let addrs = resolver.resolve_checked(name.as_str()).await?;
            // Port 0 is replaced by the URL's port.
            let addrs: reqwest::dns::Addrs =
                Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::filter;

    #[test]
    fn forced_ipv4_keeps_only_a_records() {
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(filter(vec![v6, v4], true), [v4]);
        assert_eq!(filter(vec![v6, v4], false), [v6, v4]);
    }
}
