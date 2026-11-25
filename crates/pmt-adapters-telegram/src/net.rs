//! Reaching the Bot API where `api.telegram.org` does not connect (spec:
//! platform-adapters, Telegram network resilience): a request whose
//! connection fails (the host does not resolve, or its address is
//! blocked) is retried on fallback addresses — first those DNS-over-HTTPS
//! discovery finds (once per connection, bounded by
//! `options.fallback_discovery_timeout_secs`), then `options.fallback_ips`
//! — with the host name kept for TLS, so the certificate is still
//! validated for it. The first path that works stays in use (sticky) for
//! the rest of the connection; a failure of that path walks the others
//! again. Only connection failures are retried: a request that left may
//! have arrived.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::PLATFORM;

/// How long one connection attempt may take, so a blackholed path leaves
/// time for the others within the connect bound.
pub(crate) const ATTEMPT_CONNECT: Duration = Duration::from_secs(10);

/// The public DNS-over-HTTPS resolvers (JSON API) asked for more
/// addresses of the API host.
pub(crate) const DOH_RESOLVERS: [&str; 2] = [
    "https://dns.google/resolve",
    "https://cloudflare-dns.com/dns-query",
];

/// A way to the Bot API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Path {
    /// The host name, resolved as usual.
    Host,
    /// One address, the host name kept for TLS.
    Ip(IpAddr),
}

/// Builds a client: the ordinary one (`None`), or one reaching `host` at
/// one address.
pub(crate) type MakeClient =
    Arc<dyn Fn(Option<(&str, IpAddr)>) -> Result<reqwest::Client, String> + Send + Sync>;

/// Where fallback addresses come from.
#[derive(Debug, Clone)]
pub(crate) struct Fallback {
    /// The API host the addresses stand in for.
    pub(crate) host: String,
    /// `options.fallback_ips`.
    pub(crate) configured: Vec<IpAddr>,
    /// The DNS-over-HTTPS resolvers asked (none: no discovery).
    pub(crate) resolvers: Vec<String>,
    /// How long discovery may take.
    pub(crate) discovery_timeout: Duration,
    /// IPv4 addresses only (`force_ipv4`).
    pub(crate) ipv4_only: bool,
}

/// The paths of one connection to the Bot API.
pub(crate) struct Transport {
    primary: reqwest::Client,
    fallback: Option<Fallback>,
    make: Option<MakeClient>,
    sticky: Mutex<Path>,
    /// Addresses discovery found (asked once per connection).
    discovered: tokio::sync::OnceCell<Vec<IpAddr>>,
    clients: Mutex<HashMap<IpAddr, reqwest::Client>>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("sticky", &self.path())
            .finish_non_exhaustive()
    }
}

impl Transport {
    /// One client and no fallback.
    pub(crate) fn direct(client: reqwest::Client) -> Self {
        Self::new(client, None, None)
    }

    /// `primary`, falling back as `fallback` says with clients from
    /// `make`.
    pub(crate) fn new(
        primary: reqwest::Client,
        fallback: Option<Fallback>,
        make: Option<MakeClient>,
    ) -> Self {
        Self {
            primary,
            fallback,
            make,
            sticky: Mutex::new(Path::Host),
            discovered: tokio::sync::OnceCell::new(),
            clients: Mutex::default(),
        }
    }

    /// The path requests take first.
    pub(crate) fn path(&self) -> Path {
        *self.sticky.lock().expect("sticky poisoned")
    }

    /// Sends the request `build` makes for a client, on the sticky path
    /// first, then on the others when that one does not connect.
    pub(crate) async fn send(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let sticky = self.path();
        let err = match self.attempt(sticky, &build).await {
            Ok(response) => return Ok(response),
            Err(err) => err,
        };
        if self.fallback.is_none() || !err.is_connect() {
            return Err(err);
        }
        warn_failed(sticky, &err);
        let mut last = err;
        for path in self.walk(sticky).await {
            match self.attempt(path, &build).await {
                Ok(response) => {
                    self.stick(path);
                    return Ok(response);
                }
                Err(err) if err.is_connect() => {
                    warn_failed(path, &err);
                    last = err;
                }
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    async fn attempt(
        &self,
        path: Path,
        build: &impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let client = match path {
            Path::Host => self.primary.clone(),
            Path::Ip(ip) => match self.client_at(ip) {
                Some(client) => client,
                // Unbuildable: the primary's own error stands.
                None => self.primary.clone(),
            },
        };
        build(&client).send().await
    }

    /// The client reaching the API host at `ip`, built once.
    fn client_at(&self, ip: IpAddr) -> Option<reqwest::Client> {
        let mut clients = self.clients.lock().expect("clients poisoned");
        if let Some(client) = clients.get(&ip) {
            return Some(client.clone());
        }
        let (fallback, make) = (self.fallback.as_ref()?, self.make.as_ref()?);
        match make(Some((&fallback.host, ip))) {
            Ok(client) => {
                clients.insert(ip, client.clone());
                Some(client)
            }
            Err(why) => {
                tracing::warn!(platform = PLATFORM, %ip, "fallback client not built: {why}");
                None
            }
        }
    }

    fn stick(&self, path: Path) {
        let before = std::mem::replace(&mut *self.sticky.lock().expect("sticky poisoned"), path);
        if before != path {
            match path {
                Path::Ip(ip) => tracing::warn!(
                    platform = PLATFORM,
                    %ip,
                    "the Bot API host did not connect; using fallback address {ip} for this connection"
                ),
                Path::Host => {
                    tracing::info!(platform = PLATFORM, "the Bot API host connects again");
                }
            }
        }
    }

    /// Every path but `tried`: the host, then discovered addresses, then
    /// the configured ones, each once.
    async fn walk(&self, tried: Path) -> Vec<Path> {
        let Some(fallback) = &self.fallback else {
            return Vec::new();
        };
        let discovered = self
            .discovered
            .get_or_init(|| discover(&self.primary, fallback))
            .await;
        let mut paths = vec![Path::Host];
        for ip in discovered.iter().chain(&fallback.configured) {
            let path = Path::Ip(*ip);
            let excluded = fallback.ipv4_only && ip.is_ipv6();
            if !excluded && !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths.retain(|p| *p != tried);
        paths
    }
}

fn warn_failed(path: Path, err: &reqwest::Error) {
    let what = match path {
        Path::Host => "the Bot API host".to_owned(),
        Path::Ip(ip) => format!("fallback address {ip}"),
    };
    tracing::warn!(
        platform = PLATFORM,
        "{what} did not connect: {}",
        causes(err)
    );
}

/// What went wrong, without the request's URL (it carries the token).
fn causes(err: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    if parts.is_empty() {
        "connection failed".to_owned()
    } else {
        parts.join(": ")
    }
}

/// Addresses of `fallback.host` the DNS-over-HTTPS resolvers know, asked
/// together and given up on after the discovery timeout.
async fn discover(client: &reqwest::Client, fallback: &Fallback) -> Vec<IpAddr> {
    if fallback.resolvers.is_empty() {
        return Vec::new();
    }
    let asks = fallback
        .resolvers
        .iter()
        .map(|url| ask(client, url, &fallback.host));
    let Ok(answers) = tokio::time::timeout(
        fallback.discovery_timeout,
        futures_util::future::join_all(asks),
    )
    .await
    else {
        tracing::warn!(
            platform = PLATFORM,
            "DNS-over-HTTPS discovery of Bot API addresses timed out after {} s",
            fallback.discovery_timeout.as_secs()
        );
        return Vec::new();
    };
    let mut found: Vec<IpAddr> = Vec::new();
    for ip in answers.into_iter().flatten() {
        if !found.contains(&ip) {
            found.push(ip);
        }
    }
    found
}

/// The A records one resolver's JSON API answers for `host`.
async fn ask(client: &reqwest::Client, url: &str, host: &str) -> Vec<IpAddr> {
    let Ok(target) = reqwest::Url::parse_with_params(url, [("name", host), ("type", "A")]) else {
        return Vec::new();
    };
    let response = client
        .get(target)
        .header("accept", "application/dns-json")
        .timeout(ATTEMPT_CONNECT)
        .send()
        .await;
    let body = match response {
        Ok(response) if response.status().is_success() => response.text().await,
        Ok(response) => {
            tracing::debug!(platform = PLATFORM, url, status = %response.status(), "DoH refused");
            return Vec::new();
        }
        Err(err) => {
            tracing::debug!(platform = PLATFORM, url, "DoH failed: {}", causes(&err));
            return Vec::new();
        }
    };
    body.ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|answer| a_records(&answer))
        .unwrap_or_default()
}

/// The addresses of a DNS JSON answer's A records.
pub(crate) fn a_records(answer: &Value) -> Vec<IpAddr> {
    answer["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| record["type"].as_u64() == Some(1))
        .filter_map(|record| record["data"].as_str()?.trim().parse().ok())
        .collect()
}

/// `builder` reaching `host` at `ip` (the URL's port is kept).
pub(crate) fn pinned(
    builder: reqwest::ClientBuilder,
    host: &str,
    ip: IpAddr,
) -> reqwest::ClientBuilder {
    builder.resolve(host, SocketAddr::new(ip, 0))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    const HOST: &str = "api.telegram.invalid";

    /// A server answering every request `200 ok` on 127.0.0.1 only.
    async fn server() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        pmt_core::spawn::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                pmt_core::spawn::spawn(async move {
                    let mut buf = [0; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        port
    }

    fn transport(configured: Vec<IpAddr>, ipv4_only: bool) -> Transport {
        let make: MakeClient = Arc::new(|at| {
            let builder = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
                .map_err(|e| e.to_string())?
                .no_proxy();
            let builder = match at {
                Some((host, ip)) => pinned(builder, host, ip),
                None => builder,
            };
            builder.build().map_err(|e| e.to_string())
        });
        let fallback = Fallback {
            host: HOST.to_owned(),
            configured,
            resolvers: Vec::new(),
            discovery_timeout: Duration::from_secs(1),
            ipv4_only,
        };
        Transport::new(make(None).unwrap(), Some(fallback), Some(make))
    }

    #[tokio::test]
    async fn the_first_working_address_stays_in_use() {
        let port = server().await;
        let url = format!("http://{HOST}:{port}/");
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // The mock listens on IPv4 only: the IPv6 loopback is refused.
        let t = transport(vec!["::1".parse().unwrap(), loopback], false);
        let response = t.send(|c| c.get(&url)).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(t.path(), Path::Ip(loopback));
        assert_eq!(t.walk(t.path()).await[0], Path::Host);
        t.send(|c| c.get(&url)).await.unwrap();
        assert_eq!(t.path(), Path::Ip(loopback));
    }

    #[tokio::test]
    async fn ipv4_only_skips_ipv6_addresses() {
        let t = transport(
            vec!["::1".parse().unwrap(), "192.0.2.7".parse().unwrap()],
            true,
        );
        assert_eq!(
            t.walk(Path::Host).await,
            [Path::Ip("192.0.2.7".parse().unwrap())]
        );
    }

    #[tokio::test]
    async fn no_fallback_returns_the_hosts_error() {
        let port = server().await;
        let url = format!("http://{HOST}:{port}/");
        let t = transport(Vec::new(), false);
        assert!(t.send(|c| c.get(&url)).await.unwrap_err().is_connect());
        assert_eq!(t.path(), Path::Host);
    }

    #[test]
    fn a_records_are_read_from_a_dns_json_answer() {
        let answer = json!({"Answer": [
            {"type": 5, "data": "alias.example."},
            {"type": 1, "data": "149.154.167.220"},
            {"type": 1, "data": "not an address"},
        ]});
        assert_eq!(
            a_records(&answer),
            ["149.154.167.220".parse::<IpAddr>().unwrap()]
        );
    }
}
