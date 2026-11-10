//! Raw byte streams for protocols the shared HTTP client does not carry
//! (the `WebSocket` connections of adapters, the relay and the `edge`
//! speech provider): TCP, optionally through a proxy (HTTP `CONNECT` over plain TCP or TLS,
//! or SOCKS5), then optionally TLS verified by the platform trust store
//! plus `network.extra_ca_file` (spec: security-boundaries, Platform trust
//! store for TLS).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

/// A byte stream, TLS or plain.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// How a stream reaches its host.
#[derive(Clone, Copy, Default)]
pub struct Route<'a> {
    /// The proxy URL (`http://`, `https://` or `socks5://`); `None`
    /// connects directly.
    pub proxy: Option<&'a str>,
    /// Extra PEM roots trusted besides the platform store.
    pub extra_ca_file: Option<&'a Path>,
    /// Connect over IPv4 only (`network.force_ipv4`; direct connections).
    pub force_ipv4: bool,
}

impl std::fmt::Debug for Route<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A proxy URL may carry credentials.
        f.debug_struct("Route")
            .field("proxy", &self.proxy.map(|_| ".."))
            .field("extra_ca_file", &self.extra_ca_file)
            .field("force_ipv4", &self.force_ipv4)
            .finish()
    }
}

/// A stream to `host:port` along `route`, wrapped in TLS for `host` when
/// `tls` is set.
///
/// # Errors
///
/// What failed: unreachable proxy or host, a refused tunnel, or TLS.
pub async fn open(
    host: &str,
    port: u16,
    tls: bool,
    route: Route<'_>,
) -> Result<Box<dyn Stream>, String> {
    let tcp: Box<dyn Stream> = match route.proxy {
        Some(proxy) => tunnel(proxy, host, port, route.extra_ca_file).await?,
        None => Box::new(direct(host, port, route.force_ipv4).await?),
    };
    if !tls {
        return Ok(tcp);
    }
    let config = crate::tls::client_config(route.extra_ca_file).map_err(|e| e.to_string())?;
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| format!("{host} is not a valid TLS name"))?;
    let stream = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS with {host} failed: {e}"))?;
    Ok(Box::new(stream))
}

async fn direct(host: &str, port: u16, force_ipv4: bool) -> Result<TcpStream, String> {
    let unreachable = |e: std::io::Error| format!("cannot reach {host}:{port}: {e}");
    if !force_ipv4 {
        return TcpStream::connect((host, port)).await.map_err(unreachable);
    }
    let addrs: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(unreachable)?
        .filter(std::net::SocketAddr::is_ipv4)
        .collect();
    if addrs.is_empty() {
        return Err(format!(
            "cannot reach {host}:{port}: no IPv4 address (network.force_ipv4)"
        ));
    }
    TcpStream::connect(addrs.as_slice())
        .await
        .map_err(unreachable)
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
            let config = crate::tls::client_config(extra_ca_file).map_err(|e| e.to_string())?;
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
