//! The shared outbound HTTP client. Every outbound caller builds its client
//! here, so `network.force_ipv4` cannot be forgotten by one of them.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use pmt_core::error::{Error, codes};

use crate::resolve::{Lookup, OutboundResolver, SystemLookup};

/// The profile settings that shape outbound connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboundPolicy {
    /// `network.force_ipv4`: resolve and connect over IPv4 only.
    pub force_ipv4: bool,
}

/// A client builder for one profile's outbound traffic; callers add their
/// own timeouts and headers before building.
pub fn client_builder(policy: OutboundPolicy) -> reqwest::ClientBuilder {
    client_builder_with_lookup(policy, Arc::new(SystemLookup))
}

/// [`client_builder`] with a substitute resolver, for tests.
pub fn client_builder_with_lookup(
    policy: OutboundPolicy,
    lookup: Arc<dyn Lookup>,
) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("prometheus/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(Arc::new(OutboundResolver {
            lookup,
            force_ipv4: policy.force_ipv4,
        }));
    if policy.force_ipv4 {
        // Also covers IPv6 literals in URLs, which skip name resolution:
        // an IPv4-bound socket cannot reach them.
        builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    } else {
        builder
    }
}

/// A ready client with the default settings.
///
/// # Errors
///
/// `net.client_init` when the HTTP stack cannot be initialised.
pub fn client(policy: OutboundPolicy) -> Result<reqwest::Client, Error> {
    client_builder(policy).build().map_err(|e| {
        Error::new(
            &codes::NET_CLIENT_INIT,
            format!("cannot build the HTTP client: {e}"),
        )
        .with_source(e)
    })
}
