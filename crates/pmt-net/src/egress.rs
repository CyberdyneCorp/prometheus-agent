//! The loopback egress proxy for sandboxed children (spec:
//! security-boundaries, Egress proxy for sandboxed children; python-bridge,
//! Capability grants enforced at the process boundary).
//!
//! A child whose OS sandbox lets it reach only this proxy's port gets
//! `HTTP_PROXY`/`HTTPS_PROXY` pointing here. The proxy serves `CONNECT
//! host:port` (TLS) and absolute-form plain HTTP requests, admits a host
//! only when one of its [`Rules`] names it, resolves the name itself,
//! checks every resolved address against the [`DestinationPolicy`]
//! (metadata endpoints never, private ranges only when granted) and
//! connects to that same address, so DNS rebinding cannot slip past the
//! check. A refusal answers `403` with `Blocked: <host> is blocked by
//! policy` and is counted.
//!
//! Each proxy runs on its own thread with its own runtime, so its owner
//! needs no async context; dropping the [`Proxy`] stops it.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::destination::DestinationPolicy;

/// Largest request head the proxy reads.
const MAX_HEAD: usize = 64 * 1024;

/// One allowed destination: a host (`*.suffix` for subdomains) and an
/// optional port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    /// `api.example.com` or `*.example.com`.
    pub host: String,
    /// `None`: every port.
    pub port: Option<u16>,
}

impl Allowed {
    /// Parses `host[:port]`.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() => Self {
                host: host.to_ascii_lowercase(),
                port: port.parse().ok(),
            },
            _ => Self {
                host: text.to_ascii_lowercase(),
                port: None,
            },
        }
    }

    fn admits(&self, host: &str, port: u16) -> bool {
        let host_ok = match self.host.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => self.host == host,
        };
        host_ok && self.port.is_none_or(|p| p == port)
    }
}

/// What a proxy lets through.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    /// Allowed destinations (empty: nothing).
    pub allowed: Vec<Allowed>,
    /// The address policy every resolved address must pass.
    pub policy: DestinationPolicy,
}

impl Rules {
    fn admits(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allowed.iter().any(|a| a.admits(&host, port))
    }
}

/// A running proxy.
#[derive(Debug)]
pub struct Proxy {
    addr: SocketAddr,
    denied: Arc<AtomicU64>,
    stop: Option<oneshot::Sender<()>>,
}

impl Proxy {
    /// Starts a proxy on an ephemeral loopback port.
    ///
    /// # Errors
    ///
    /// When the listener cannot bind or the thread cannot start.
    pub fn start(rules: Rules) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let denied = Arc::new(AtomicU64::new(0));
        let (stop, stopped) = oneshot::channel();
        let counter = Arc::clone(&denied);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("egress-proxy".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    let rules = Arc::new(rules);
                    tokio::select! {
                        () = accept(listener, rules, counter) => {}
                        _ = stopped => {}
                    }
                });
            })?;
        Ok(Self {
            addr,
            denied,
            stop: Some(stop),
        })
    }

    /// The listening address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`, for `HTTP_PROXY`/`HTTPS_PROXY`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Connections refused so far.
    #[must_use]
    pub fn denied(&self) -> u64 {
        self.denied.load(Ordering::Relaxed)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn accept(listener: TcpListener, rules: Arc<Rules>, denied: Arc<AtomicU64>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (rules, denied) = (Arc::clone(&rules), Arc::clone(&denied));
        tokio::spawn(async move {
            if serve(stream, &rules).await == Err(true) {
                denied.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
}

/// The request line of a proxied request.
struct Target {
    connect: bool,
    host: String,
    port: u16,
    /// The request head rewritten to origin form (plain HTTP).
    head: Vec<u8>,
}

/// Serves one client connection; `Err(true)` when it was refused by
/// policy, `Err(false)` on any other failure.
async fn serve(mut client: TcpStream, rules: &Rules) -> Result<(), bool> {
    let (head, rest) = read_head(&mut client).await.map_err(|()| false)?;
    let Some(target) = parse(&head) else {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
        return Err(false);
    };
    let upstream = match open(rules, &target.host, target.port).await {
        Ok(upstream) => upstream,
        Err(why) => {
            let body = format!("Blocked: {} is blocked by policy ({why})\n", target.host);
            let reply = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = client.write_all(reply.as_bytes()).await;
            return Err(true);
        }
    };
    relay(client, upstream, &target, &rest)
        .await
        .map_err(|_| false)
}

async fn relay(
    mut client: TcpStream,
    mut upstream: TcpStream,
    target: &Target,
    rest: &[u8],
) -> io::Result<()> {
    if target.connect {
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
    } else {
        upstream.write_all(&target.head).await?;
    }
    upstream.write_all(rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Reads up to the end of the request head; returns it and any bytes read
/// past it.
async fn read_head(client: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_HEAD {
            return Err(());
        }
    }
}

/// The target of a request head.
fn parse(head: &[u8]) -> Option<Target> {
    let text = std::str::from_utf8(head).ok()?;
    let (line, headers) = text.split_once("\r\n")?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host(target, 443)?;
        return Some(Target {
            connect: true,
            host,
            port,
            head: Vec::new(),
        });
    }
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |at| (&rest[..at], &rest[at..]));
    let (host, port) = split_host(authority, 80)?;
    let mut kept = String::new();
    for header in headers
        .split("\r\n")
        .filter(|h| !h.to_ascii_lowercase().starts_with("proxy-"))
    {
        kept.push_str(header);
        kept.push_str("\r\n");
    }
    Some(Target {
        connect: false,
        host,
        port,
        head: format!("{method} {path} {version}\r\n{kept}").into_bytes(),
    })
}

/// `host[:port]` (IPv6 in brackets).
fn split_host(authority: &str, default: u16) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after
            .strip_prefix(':')
            .map_or(Some(default), |p| p.parse().ok())?;
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
        None => Some((authority.to_owned(), default)),
    }
}

/// Checks `host:port` and connects to the first admitted address.
async fn open(rules: &Rules, host: &str, port: u16) -> Result<TcpStream, String> {
    if !rules.admits(host, port) {
        return Err("not granted".to_owned());
    }
    rules.policy.check_host(host).map_err(|r| r.to_string())?;
    let literal = host.parse::<IpAddr>().ok();
    let addrs: Vec<SocketAddr> = match literal {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| "did not resolve".to_owned())?
            .collect(),
    };
    let named = literal.is_none().then_some(host);
    let mut last = "did not resolve".to_owned();
    for addr in addrs {
        if let Err(refusal) = rules.policy.check_addr(named, addr.ip(), Some(port)) {
            last = refusal.to_string();
            continue;
        }
        // Connect to the address that was checked, never re-resolve.
        return TcpStream::connect(addr).await.map_err(|e| e.to_string());
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::destination::PrivateGrant;

    fn server() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn ask(proxy: &Proxy, request: &str) -> String {
        let mut stream = std::net::TcpStream::connect(proxy.addr()).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            answer.extend_from_slice(&chunk[..n]);
            if answer.windows(4).any(|w| w == b"\r\n\r\n") && !request.starts_with("GET") {
                break;
            }
            if String::from_utf8_lossy(&answer).contains("hello") {
                break;
            }
        }
        String::from_utf8_lossy(&answer).into_owned()
    }

    #[test]
    fn a_granted_host_is_reached_and_another_is_blocked() {
        let (listener, port) = server();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).unwrap();
            assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("GET /path HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        });
        let rules = Rules {
            allowed: vec![Allowed::parse("127.0.0.1")],
            policy: DestinationPolicy::default()
                .with_grant(PrivateGrant::parse("127.0.0.1").unwrap()),
        };
        let proxy = Proxy::start(rules).unwrap();
        let answer = ask(
            &proxy,
            &format!("GET http://127.0.0.1:{port}/path HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(answer.contains("hello"), "{answer}");
        let refused = ask(&proxy, "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(proxy.denied(), 1);
    }

    #[test]
    fn a_granted_name_resolving_to_a_private_address_is_refused() {
        let rules = Rules {
            allowed: vec![Allowed::parse("localhost")],
            policy: DestinationPolicy::default(),
        };
        let proxy = Proxy::start(rules).unwrap();
        let refused = ask(&proxy, "CONNECT localhost:443 HTTP/1.1\r\n\r\n");
        assert!(
            refused.contains("Blocked: localhost is blocked by policy"),
            "{refused}"
        );
    }
}
