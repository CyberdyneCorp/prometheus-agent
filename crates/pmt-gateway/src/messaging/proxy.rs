//! Adapter proxies and environment trust (spec: platform-adapters, Adapter
//! proxy settings and environment trust): an adapter's HTTP and WebSocket
//! traffic goes through `gateway.platforms.<name>.proxy_url` when set,
//! else `gateway.proxy_url`, else nowhere. Inherited proxy environment
//! variables count only with `gateway.trust_env = true`; otherwise a
//! daemon started with `HTTPS_PROXY` set still connects directly. A proxy
//! URL with credentials comes from the secrets store
//! (`proxy_url_secret`).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use pmt_config::schema::{GatewayConfig, PlatformConfig};
use pmt_core::error::{Error, codes};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

use super::adapter::{AdapterError, AdapterSetup};
use super::ws::Stream;

/// How an adapter reaches the network.
#[derive(Clone, PartialEq, Eq)]
pub enum Proxy {
    /// Directly; ambient proxy settings are ignored.
    Direct,
    /// Through the proxy the environment names (`gateway.trust_env`).
    Environment,
    /// Through this proxy (`http://`, `https://` or `socks5://`).
    Url(String),
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A proxy URL may carry credentials.
        match self {
            Self::Direct => f.write_str("Direct"),
            Self::Environment => f.write_str("Environment"),
            Self::Url(_) => f.write_str("Url(..)"),
        }
    }
}

impl Proxy {
    /// The proxy of `platform`'s table under `gateway`, reading a
    /// `proxy_url_secret` with `secret`.
    ///
    /// # Errors
    ///
    /// The named secret is missing or unreadable.
    pub fn resolve(
        platform: &PlatformConfig,
        gateway: &GatewayConfig,
        secret: &dyn Fn(&str) -> Result<String, AdapterError>,
    ) -> Result<Self, AdapterError> {
        if let Some(name) = &platform.proxy_url_secret {
            return secret(&name.0).map(Self::Url);
        }
        if let Some(url) = &platform.proxy_url {
            return Ok(Self::Url(url.clone()));
        }
        if let Some(name) = &gateway.proxy_url_secret {
            return secret(&name.0).map(Self::Url);
        }
        if let Some(url) = &gateway.proxy_url {
            return Ok(Self::Url(url.clone()));
        }
        Ok(if gateway.trust_env {
            Self::Environment
        } else {
            Self::Direct
        })
    }

    /// The proxy of the adapter `setup` describes.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve).
    pub fn of(setup: &AdapterSetup) -> Result<Self, AdapterError> {
        let secret = |name: &str| super::adapter::credential(&setup.scope, name);
        Self::resolve(&setup.config, &setup.scope.config().gateway, &secret)
    }

    /// The proxy a WebSocket to a `tls` (or plain) URL goes through:
    /// the configured one, or under environment trust the one the
    /// environment names.
    #[must_use]
    pub fn for_websocket(&self, tls: bool) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::Url(url) => Some(url.clone()),
            Self::Environment => {
                let names: &[&str] = if tls {
                    &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                } else {
                    &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
                };
                names
                    .iter()
                    .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
            }
        }
    }
}

/// An HTTP client builder for the adapter `setup` describes: the profile's
/// TLS roots and IPv4 rule, and the adapter's proxy (direct unless one is
/// configured or the environment is trusted).
///
/// # Errors
///
/// `net.client_init` / `net.extra_ca`; a proxy secret that cannot be read.
pub fn client_builder(setup: &AdapterSetup) -> Result<(reqwest::ClientBuilder, Proxy), Error> {
    let proxy = Proxy::of(setup).map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.message))?;
    let network = &setup.scope.config().network;
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_deref().map(Into::into),
        proxy: match &proxy {
            Proxy::Url(url) => Some(url.clone()),
            Proxy::Direct | Proxy::Environment => None,
        },
    };
    let builder = pmt_net::http::client_builder(&policy)?;
    let builder = match proxy {
        // An explicit proxy already replaces the environment's.
        Proxy::Direct => builder.no_proxy(),
        Proxy::Environment | Proxy::Url(_) => builder,
    };
    Ok((builder, proxy))
}

/// A byte stream to `host:port` through the proxy at `url` (HTTP
/// `CONNECT` over plain TCP or TLS, or SOCKS5).
///
/// # Errors
///
/// The proxy is unreachable, refuses or speaks another protocol.
pub async fn tunnel(
    url: &str,
    host: &str,
    port: u16,
    extra_ca_file: Option<&Path>,
) -> Result<Box<dyn Stream>, String> {
    let parsed = ProxyUrl::parse(url)?;
    let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .map_err(|e| {
            format!(
                "cannot reach the proxy {}:{}: {e}",
                parsed.host, parsed.port
            )
        })?;
    match parsed.scheme.as_str() {
        "socks5" => socks5(Box::new(tcp), &parsed, host, port).await,
        "https" => {
            let config = pmt_net::tls::client_config(extra_ca_file).map_err(|e| e.to_string())?;
            let name = ServerName::try_from(parsed.host.clone())
                .map_err(|_| format!("{} is not a valid TLS name", parsed.host))?;
            let tls = TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS with the proxy failed: {e}"))?;
            connect(Box::new(tls), &parsed, host, port).await
        }
        _ => connect(Box::new(tcp), &parsed, host, port).await,
    }
}

/// The parts of a proxy URL.
struct ProxyUrl {
    scheme: String,
    host: String,
    port: u16,
    user: Option<(String, String)>,
}

impl ProxyUrl {
    fn parse(url: &str) -> Result<Self, String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| "the proxy is not a URL".to_owned())?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((info, hostport)) => {
                let (name, password) = info.split_once(':').unwrap_or((info, ""));
                (Some((name.to_owned(), password.to_owned())), hostport)
            }
            None => (None, authority),
        };
        let default_port = match scheme {
            "https" => 443,
            "socks5" => 1080,
            _ => 80,
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) if !host.ends_with(']') || hostport.starts_with('[') => (
                host,
                port.parse().map_err(|_| "the proxy port is not a number")?,
            ),
            _ => (hostport, default_port),
        };
        Ok(Self {
            scheme: scheme.to_owned(),
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            user,
        })
    }
}

/// An HTTP `CONNECT` tunnel.
async fn connect(
    mut stream: Box<dyn Stream>,
    proxy: &ProxyUrl,
    host: &str,
    port: u16,
) -> Result<Box<dyn Stream>, String> {
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let authorization = proxy
        .user
        .as_ref()
        .map_or_else(String::new, |(name, password)| {
            let token =
                base64::engine::general_purpose::STANDARD.encode(format!("{name}:{password}"));
            format!("Proxy-Authorization: Basic {token}\r\n")
        });
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{authorization}\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("the proxy connection failed: {e}"))?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| format!("the proxy connection failed: {e}"))?;
        if n == 0 || head.len() > 8192 {
            return Err("the proxy closed the connection".to_owned());
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let line = status.lines().next().unwrap_or_default().to_owned();
    if line.split_whitespace().nth(1) == Some("200") {
        Ok(stream)
    } else {
        Err(format!("the proxy refused the tunnel: {line}"))
    }
}

/// A SOCKS5 tunnel (no authentication, or user name and password).
async fn socks5(
    mut stream: Box<dyn Stream>,
    proxy: &ProxyUrl,
    host: &str,
    port: u16,
) -> Result<Box<dyn Stream>, String> {
    let io = |e: std::io::Error| format!("the SOCKS5 proxy connection failed: {e}");
    let method = if proxy.user.is_some() { 2u8 } else { 0u8 };
    stream.write_all(&[5, 1, method]).await.map_err(io)?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await.map_err(io)?;
    if choice != [5, method] {
        return Err("the SOCKS5 proxy refused the authentication method".to_owned());
    }
    if let Some((name, password)) = &proxy.user {
        let mut auth = vec![
            1,
            u8::try_from(name.len()).map_err(|_| "user name too long")?,
        ];
        auth.extend_from_slice(name.as_bytes());
        auth.push(u8::try_from(password.len()).map_err(|_| "password too long")?);
        auth.extend_from_slice(password.as_bytes());
        stream.write_all(&auth).await.map_err(io)?;
        let mut status = [0u8; 2];
        stream.read_exact(&mut status).await.map_err(io)?;
        if status[1] != 0 {
            return Err("the SOCKS5 proxy refused the credentials".to_owned());
        }
    }
    let mut request = vec![
        5,
        1,
        0,
        3,
        u8::try_from(host.len()).map_err(|_| "host too long")?,
    ];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(io)?;
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.map_err(io)?;
    if reply[1] != 0 {
        return Err(format!(
            "the SOCKS5 proxy refused the connection (code {})",
            reply[1]
        ));
    }
    let skip = match reply[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(io)?;
            usize::from(len[0]) + 2
        }
        _ => return Err("the SOCKS5 proxy sent an unknown address type".to_owned()),
    };
    let mut bound = vec![0u8; skip];
    stream.read_exact(&mut bound).await.map_err(io)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_secret(name: &str) -> Result<String, AdapterError> {
        Err(AdapterError {
            retryable: false,
            code: "missing_secret".into(),
            message: name.into(),
        })
    }

    #[test]
    fn the_platform_proxy_wins_then_the_shared_one_then_direct() {
        let gateway = GatewayConfig {
            proxy_url: Some("socks5://shared:1080".into()),
            ..GatewayConfig::default()
        };
        let platform = PlatformConfig {
            proxy_url: Some("http://own:3128".into()),
            ..PlatformConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&platform, &gateway, &no_secret).unwrap(),
            Proxy::Url("http://own:3128".into())
        );
        assert_eq!(
            Proxy::resolve(&PlatformConfig::default(), &gateway, &no_secret).unwrap(),
            Proxy::Url("socks5://shared:1080".into())
        );
        let secret = |_: &str| Ok("http://u:pw@vault:3128".to_owned());
        let platform = PlatformConfig {
            proxy_url_secret: Some(pmt_config::schema::SecretName("PROXY".into())),
            ..PlatformConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&platform, &GatewayConfig::default(), &secret).unwrap(),
            Proxy::Url("http://u:pw@vault:3128".into())
        );
    }

    #[test]
    fn ambient_proxy_settings_count_only_with_trust_env() {
        let direct = Proxy::resolve(
            &PlatformConfig::default(),
            &GatewayConfig::default(),
            &no_secret,
        )
        .unwrap();
        assert_eq!(direct, Proxy::Direct);
        // Whatever the environment holds, a direct adapter tunnels nowhere.
        assert_eq!(direct.for_websocket(true), None);
        let trusting = GatewayConfig {
            trust_env: true,
            ..GatewayConfig::default()
        };
        assert_eq!(
            Proxy::resolve(&PlatformConfig::default(), &trusting, &no_secret).unwrap(),
            Proxy::Environment
        );
    }

    #[test]
    fn proxy_urls_parse_with_credentials_and_default_ports() {
        let url = ProxyUrl::parse("http://u:p%40ss@proxy.example:3128").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("proxy.example", 3128));
        assert_eq!(url.user, Some(("u".into(), "p%40ss".into())));
        let socks = ProxyUrl::parse("socks5://127.0.0.1").unwrap();
        assert_eq!((socks.scheme.as_str(), socks.port), ("socks5", 1080));
        let v6 = ProxyUrl::parse("https://[::1]:8443/").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8443));
    }
}
