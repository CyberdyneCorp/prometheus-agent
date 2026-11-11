//! Adapter connectivity probes (spec: observability, Configuration-centric
//! status): `prometheus status --deep` asks each enabled adapter where it
//! connects out to ([`Adapter::reach`](super::adapter::Adapter::reach))
//! and checks every endpoint within a deadline, the way the adapter
//! reaches it: an HTTP request through the adapter's own client (its
//! proxy, TLS roots and IPv4 rule) for platform APIs, a direct request
//! for loopback services, a TCP connection for mail servers. Any HTTP
//! answer counts: the probe tests connectivity, not credentials.

use std::time::{Duration, Instant};

use super::adapter::AdapterSetup;

/// Where an adapter connects out to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// It only receives (webhooks): nothing to probe.
    InboundOnly,
    /// It connects to these endpoints.
    Endpoints(Vec<Endpoint>),
}

/// One endpoint an adapter connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A platform API (`https://`, `http://`, `wss://` or `ws://`),
    /// reached through the adapter's proxy when one applies.
    Http(String),
    /// A local service's HTTP URL (a bridge on loopback), always reached
    /// directly.
    LocalHttp(String),
    /// A plain TCP service: host and port (mail servers).
    Tcp(String, u16),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(url) | Self::LocalHttp(url) => f.write_str(url),
            Self::Tcp(host, port) => write!(f, "{host}:{port}"),
        }
    }
}

/// What a probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probed {
    /// Nothing to probe.
    InboundOnly,
    /// Every endpoint answered; the slowest took this long.
    Reached(Duration),
}

/// Probes every endpoint of `reach` for the adapter `setup` describes,
/// each within `within`.
///
/// # Errors
///
/// The first endpoint that did not answer, named, with why.
pub async fn probe(
    setup: &AdapterSetup,
    reach: &Reach,
    within: Duration,
) -> Result<Probed, String> {
    let Reach::Endpoints(endpoints) = reach else {
        return Ok(Probed::InboundOnly);
    };
    if endpoints.is_empty() {
        return Err("the adapter declares no endpoint".to_owned());
    }
    let mut slowest = Duration::ZERO;
    for endpoint in endpoints {
        let took = one(setup, endpoint, within)
            .await
            .map_err(|why| format!("{endpoint}: {why}"))?;
        slowest = slowest.max(took);
    }
    Ok(Probed::Reached(slowest))
}

async fn one(
    setup: &AdapterSetup,
    endpoint: &Endpoint,
    within: Duration,
) -> Result<Duration, String> {
    let started = Instant::now();
    let outcome = match endpoint {
        Endpoint::Http(url) => {
            let (builder, _) =
                super::proxy::client_builder(setup).map_err(|e| e.message().to_owned())?;
            http(builder, url, within).await
        }
        Endpoint::LocalHttp(url) => {
            let builder = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
                .map_err(|e| e.message().to_owned())?
                .no_proxy();
            http(builder, url, within).await
        }
        Endpoint::Tcp(host, port) => tcp(host, *port, within).await,
    };
    outcome.map(|()| started.elapsed())
}

async fn http(builder: reqwest::ClientBuilder, url: &str, within: Duration) -> Result<(), String> {
    let client = builder
        .timeout(within)
        .build()
        .map_err(|e| format!("cannot build a client: {e}"))?;
    // A WebSocket endpoint answers plain HTTP on the same scheme family.
    let url = url
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            url.strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or_else(|| url.to_owned());
    client
        .get(&url)
        .send()
        .await
        .map(drop)
        .map_err(|e| describe(&e, within))
}

fn describe(err: &reqwest::Error, within: Duration) -> String {
    if err.is_timeout() {
        return format!("no answer within {}s", within.as_secs());
    }
    // The innermost cause says why (refused, unresolvable, TLS).
    let mut why = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        why = cause.to_string();
        source = cause.source();
    }
    why
}

async fn tcp(host: &str, port: u16, within: Duration) -> Result<(), String> {
    match tokio::time::timeout(within, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("cannot connect: {e}")),
        Err(_) => Err(format!("no answer within {}s", within.as_secs())),
    }
}
