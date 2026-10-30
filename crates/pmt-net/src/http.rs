//! The shared outbound HTTP client. Every outbound caller builds its client
//! here, so `network.force_ipv4` and platform TLS verification
//! (`network.extra_ca_file`) cannot be forgotten by one of them.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Outbound clients configured by this process (spec: observability, No
/// outbound telemetry by default: tests assert that a session configures
/// none unless something was configured to reach the network).
static CLIENTS_BUILT: AtomicUsize = AtomicUsize::new(0);

/// How many outbound HTTP clients this process has configured.
#[must_use]
pub fn clients_built() -> usize {
    CLIENTS_BUILT.load(Ordering::Relaxed)
}

/// The builder with TLS, user agent, resolver and IPv4-only binding.
pub(crate) fn base_builder(
    policy: &OutboundPolicy,
    resolver: OutboundResolver,
) -> Result<reqwest::ClientBuilder, Error> {
    CLIENTS_BUILT.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(component = "net", "outbound HTTP client configured");
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::{OutboundPolicy, client, clients_built};

    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The record and count the no-telemetry tests look for.
    #[test]
    fn every_configured_client_is_counted_and_logged_at_debug() {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || Sink(Arc::clone(&sink)))
            .finish();
        let before = clients_built();
        tracing::subscriber::with_default(subscriber, || {
            client(&OutboundPolicy::default()).unwrap();
        });
        assert!(clients_built() > before);
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(text.contains("outbound HTTP client configured"), "{text}");
    }
}
