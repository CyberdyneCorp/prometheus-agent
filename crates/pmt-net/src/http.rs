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
    /// `network.proxy`: the HTTP(S) proxy every request goes through.
    pub proxy: Option<String>,
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

/// [`client_builder`] whose `network.proxy` (or, without it, the proxy
/// variables) never carries a request to a loopback host: model servers on this machine (llama.cpp, Ollama) are
/// reached directly, so a prompt meant for them cannot leave the machine
/// through the proxy (spec: local-models, Local inference stays local).
/// Other callers keep proxying loopback, which is what a proxy set for
/// them asks for.
///
/// # Errors
///
/// As [`client_builder`].
pub fn client_builder_loopback_direct(
    policy: &OutboundPolicy,
) -> Result<reqwest::ClientBuilder, Error> {
    let resolver = OutboundResolver {
        lookup: Arc::new(SystemLookup),
        force_ipv4: policy.force_ipv4,
        destinations: None,
    };
    builder_with_identity(policy, resolver, None, Loopback::Direct)
}

/// Whether `network.proxy` carries loopback requests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loopback {
    Proxied,
    Direct,
}

/// The hosts [`Loopback::Direct`] exempts from the proxy.
const LOOPBACK_HOSTS: &str = "localhost,127.0.0.0/8,::1";

/// A `reqwest::Proxy` constructor for one scheme.
type MakeProxy = fn(String) -> reqwest::Result<reqwest::Proxy>;

/// The proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, either
/// case) with `NO_PROXY`, as reqwest reads them, except that a loopback
/// host is never proxied (as Go's `net/http` does). Empty when none is set,
/// which leaves reqwest's own lookup in place.
fn env_proxies_except_loopback() -> Vec<reqwest::Proxy> {
    let var = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .ok()
            .filter(|v| !v.is_empty())
    };
    let exempt = match var("NO_PROXY") {
        Some(list) => format!("{LOOPBACK_HOSTS},{list}"),
        None => LOOPBACK_HOSTS.to_owned(),
    };
    let schemes: [(&str, MakeProxy); 3] = [
        ("HTTPS_PROXY", reqwest::Proxy::https),
        ("HTTP_PROXY", reqwest::Proxy::http),
        ("ALL_PROXY", reqwest::Proxy::all),
    ];
    schemes
        .into_iter()
        .filter_map(|(name, make)| {
            let url = var(name)?;
            match make(url) {
                Ok(proxy) => Some(proxy.no_proxy(reqwest::NoProxy::from_string(&exempt))),
                Err(err) => {
                    tracing::warn!(component = "net", variable = name, error = %err, "ignored an invalid proxy variable");
                    None
                }
            }
        })
        .collect()
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
    builder_with_identity(policy, resolver, None, Loopback::Proxied)
}

fn builder_with_identity(
    policy: &OutboundPolicy,
    resolver: OutboundResolver,
    identity: Option<crate::identity::ClientIdentity>,
    loopback: Loopback,
) -> Result<reqwest::ClientBuilder, Error> {
    CLIENTS_BUILT.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(component = "net", "outbound HTTP client configured");
    let tls = crate::tls::client_config_with(policy.extra_ca_file.as_deref(), identity)?;
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("prometheus/", env!("CARGO_PKG_VERSION")))
        .tls_backend_preconfigured(tls)
        .dns_resolver(Arc::new(resolver));
    if let Some(url) = &policy.proxy {
        let proxy = reqwest::Proxy::all(url).map_err(|e| {
            Error::new(
                &codes::NET_CLIENT_INIT,
                format!("network.proxy {url} is not a proxy URL: {e}"),
            )
        })?;
        let proxy = match loopback {
            Loopback::Proxied => proxy,
            Loopback::Direct => proxy.no_proxy(reqwest::NoProxy::from_string(LOOPBACK_HOSTS)),
        };
        builder = builder.proxy(proxy);
    } else if loopback == Loopback::Direct {
        for proxy in env_proxies_except_loopback() {
            builder = builder.proxy(proxy);
        }
    }
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

/// A ready client presenting `identity` to servers that ask for a client
/// certificate (mutual TLS).
///
/// # Errors
///
/// As [`client`].
pub fn client_with_identity(
    policy: &OutboundPolicy,
    identity: crate::identity::ClientIdentity,
) -> Result<reqwest::Client, Error> {
    let resolver = OutboundResolver {
        lookup: Arc::new(SystemLookup),
        force_ipv4: policy.force_ipv4,
        destinations: None,
    };
    build(builder_with_identity(
        policy,
        resolver,
        Some(identity),
        Loopback::Proxied,
    )?)
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
