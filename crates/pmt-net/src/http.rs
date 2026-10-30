//! The shared outbound HTTP client. Every outbound caller builds its client
//! here, so `network.force_ipv4` and platform TLS verification
//! (`network.extra_ca_file`) cannot be forgotten by one of them.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

use pmt_core::error::{Error, codes};

use crate::resolve::{Lookup, OutboundResolver, SystemLookup};

/// The profile settings that shape outbound connections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundPolicy {
    /// `network.force_ipv4`: resolve and connect over IPv4 only.
    pub force_ipv4: bool,
    /// `network.extra_ca_file`: PEM roots trusted besides the OS store.
    pub extra_ca_file: Option<PathBuf>,
}

/// A client builder for one profile's outbound traffic; callers add their
/// own timeouts and headers before building.
///
/// # Errors
///
/// `net.extra_ca` / `net.client_init` from [`crate::tls::client_config`].
pub fn client_builder(policy: &OutboundPolicy) -> Result<reqwest::ClientBuilder, Error> {
    client_builder_with_lookup(policy, Arc::new(SystemLookup))
}

/// [`client_builder`] with a substitute resolver, for tests.
///
/// # Errors
///
/// As [`client_builder`].
pub fn client_builder_with_lookup(
    policy: &OutboundPolicy,
    lookup: Arc<dyn Lookup>,
) -> Result<reqwest::ClientBuilder, Error> {
    let resolver = OutboundResolver {
        lookup,
        force_ipv4: policy.force_ipv4,
        destinations: None,
    };
    base_builder(policy, resolver)
}

/// The builder with TLS, user agent, resolver and IPv4-only binding.
pub(crate) fn base_builder(
    policy: &OutboundPolicy,
    resolver: OutboundResolver,
) -> Result<reqwest::ClientBuilder, Error> {
    let tls = crate::tls::client_config(policy.extra_ca_file.as_deref())?;
    let builder = reqwest::Client::builder()
        .user_agent(concat!("prometheus/", env!("CARGO_PKG_VERSION")))
        .tls_backend_preconfigured(tls)
        .dns_resolver(Arc::new(resolver));
    Ok(if policy.force_ipv4 {
        // Also covers IPv6 literals in URLs, which skip name resolution:
        // an IPv4-bound socket cannot reach them.
        builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    } else {
        builder
    })
}

/// A ready client with the default settings.
///
/// # Errors
///
/// `net.client_init` when the HTTP stack cannot be initialised;
/// `net.extra_ca` for a bad `network.extra_ca_file`.
pub fn client(policy: &OutboundPolicy) -> Result<reqwest::Client, Error> {
    build(client_builder(policy)?)
}

/// Builds `builder`, mapping failure to `net.client_init`.
pub(crate) fn build(builder: reqwest::ClientBuilder) -> Result<reqwest::Client, Error> {
    builder.build().map_err(|e| {
        Error::new(
            &codes::NET_CLIENT_INIT,
            format!("cannot build the HTTP client: {e}"),
        )
        .with_source(e)
    })
}
