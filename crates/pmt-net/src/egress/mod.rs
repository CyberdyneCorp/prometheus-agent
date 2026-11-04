//! The loopback egress proxy for sandboxed children (spec:
//! security-boundaries, Egress enforcement for child processes; Per-session
//! network egress policy; python-bridge, Capability grants enforced at the
//! process boundary).
//!
//! A child whose OS sandbox lets it reach only this proxy's port gets
//! `HTTP_PROXY`/`HTTPS_PROXY` pointing here. The proxy serves `CONNECT
//! host:port` (TLS) and absolute-form plain HTTP requests. It admits a host
//! only when the session's [`HostPolicy`] and the requester's own grants
//! (when it has any) both name it, resolves the name itself, checks every
//! resolved address against the [`DestinationPolicy`] (metadata endpoints
//! never, private ranges only when allowed or granted) and connects to
//! that same address, so DNS rebinding cannot slip past the check. A
//! refusal answers `403` with `Blocked: <host> is blocked by policy`, is
//! counted and is reported to the proxy's observer (an `egress.denied`
//! trajectory event and an audit record).
//!
//! Each proxy runs on its own thread with its own runtime, so its owner
//! needs no async context; dropping the [`Proxy`] stops it.

mod policy;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

pub use self::policy::{Allowed, Denial, HostPolicy, Mode, Reason, State};
use crate::destination::DestinationPolicy;
use crate::resolve::{Lookup, SystemLookup};

/// The variables that point a child's HTTP clients at the proxy.
pub const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Largest request head the proxy reads.
const MAX_HEAD: usize = 64 * 1024;

/// How long the proxy waits for an upstream connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Receives every refused connection.
pub type Observer = Arc<dyn Fn(Denial) + Send + Sync>;

/// What a proxy lets through.
#[derive(Clone)]
pub struct Rules {
    /// The session's mode and patterns.
    pub hosts: Arc<HostPolicy>,
    /// The requester's own grants (`net.connect` hosts of a plugin);
    /// `None`: the session policy alone decides.
    pub granted: Option<Vec<Allowed>>,
    /// The address policy every resolved address must pass.
    pub policy: DestinationPolicy,
    /// Who the proxied connections belong to (`terminal`, `plugin:<id>`).
    pub requester: String,
    /// The resolver (the system's; tests substitute answers).
    pub lookup: Arc<dyn Lookup>,
    /// Told about every refusal.
    pub observer: Option<Observer>,
}

impl Rules {
    /// Rules of `requester` under the session policy `hosts`, with the
    /// system resolver and no observer.
    #[must_use]
    pub fn new(hosts: Arc<HostPolicy>, policy: DestinationPolicy, requester: &str) -> Self {
        Self {
            hosts,
            granted: None,
            policy,
            requester: requester.to_owned(),
            lookup: Arc::new(SystemLookup),
            observer: None,
        }
    }

    /// Narrowed to the hosts `granted` names as well.
    #[must_use]
    pub fn granted(mut self, granted: Vec<Allowed>) -> Self {
        self.granted = Some(granted);
        self
    }

    /// Reporting each refusal to `observer`.
    #[must_use]
    pub fn observed(mut self, observer: Observer) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Resolving with `lookup`.
    #[must_use]
    pub fn with_lookup(mut self, lookup: Arc<dyn Lookup>) -> Self {
        self.lookup = lookup;
        self
    }

    /// Whether the session policy and the grants admit `host:port`.
    fn admits(&self, host: &str, port: u16) -> Result<(), Reason> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(granted) = &self.granted
            && !granted.iter().any(|a| a.admits(&host, port))
        {
            return Err(Reason::NotGranted);
        }
        self.hosts.check(&host, port)
    }
}

impl std::fmt::Debug for Rules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rules")
            .field("hosts", &self.hosts)
            .field("granted", &self.granted)
            .field("requester", &self.requester)
            .finish_non_exhaustive()
    }
}

/// A running proxy.
#[derive(Debug)]
pub struct Proxy {
    addr: SocketAddr,
    denied: Arc<AtomicU64>,
    stop: Option<oneshot::Sender<()>>,
    /// For [`Proxy::attacher`] (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    runtime: tokio::runtime::Handle,
    #[cfg_attr(not(unix), allow(dead_code))]
    rules: Arc<Rules>,
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
        let handle = runtime.handle().clone();
        let rules = Arc::new(rules);
        let served = Arc::clone(&rules);
        std::thread::Builder::new()
            .name("egress-proxy".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    tokio::select! {
                        () = accept(listener, served, counter) => {}
                        _ = stopped => {}
                    }
                });
            })?;
        Ok(Self {
            addr,
            denied,
            stop: Some(stop),
            runtime: handle,
            rules,
        })
    }

    /// Serves listeners other than its own with the same rules: the
    /// private listener of a child confined to its own network namespace
    /// (`pmt_sandbox::relay`).
    #[cfg(unix)]
    #[must_use]
    pub fn attacher(&self) -> Attacher {
        Attacher {
            runtime: self.runtime.clone(),
            rules: Arc::clone(&self.rules),
            denied: Arc::clone(&self.denied),
        }
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

/// Hands extra listeners to a running [`Proxy`]; they stop with it.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct Attacher {
    runtime: tokio::runtime::Handle,
    rules: Arc<Rules>,
    denied: Arc<AtomicU64>,
}

#[cfg(unix)]
impl Attacher {
    /// Serves `listener` until `alive` reads end-of-file (every process
    /// holding its peer has exited) or the proxy stops.
    pub fn serve(&self, listener: std::net::TcpListener, alive: std::os::unix::net::UnixStream) {
        let (rules, denied) = (Arc::clone(&self.rules), Arc::clone(&self.denied));
        drop(self.runtime.spawn(async move {
            let ready = listener
                .set_nonblocking(true)
                .and(alive.set_nonblocking(true));
            let (Ok(()), Ok(listener), Ok(alive)) = (
                ready,
                TcpListener::from_std(listener),
                tokio::net::UnixStream::from_std(alive),
            ) else {
                return;
            };
            tokio::select! {
                () = accept(listener, rules, denied) => {}
                () = closed(alive) => {}
            }
        }));
    }
}

/// Returns once `alive` reads end-of-file.
#[cfg(unix)]
async fn closed(mut alive: tokio::net::UnixStream) {
    let mut byte = [0u8; 1];
    while matches!(alive.read(&mut byte).await, Ok(n) if n > 0) {}
}

async fn accept(listener: TcpListener, rules: Arc<Rules>, denied: Arc<AtomicU64>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (rules, denied) = (Arc::clone(&rules), Arc::clone(&denied));
        tokio::spawn(async move {
            if let Err(Some(denial)) = serve(stream, &rules).await {
                denied.fetch_add(1, Ordering::Relaxed);
                if let Some(observer) = &rules.observer {
                    observer(denial);
                }
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

/// Serves one client connection; `Err(Some(denial))` when the policy
/// refused it, `Err(None)` on any other failure.
async fn serve(mut client: TcpStream, rules: &Rules) -> Result<(), Option<Denial>> {
    let (head, rest) = read_head(&mut client).await.map_err(|()| None)?;
    let Some(target) = parse(&head) else {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
        return Err(None);
    };
    let upstream = match open(rules, &target.host, target.port).await {
        Ok(upstream) => upstream,
        Err((reason, why)) => {
            let (status, body) = if reason.is_policy() {
                (
                    "403 Forbidden",
                    format!("Blocked: {} is blocked by policy ({why})\n", target.host),
                )
            } else {
                ("502 Bad Gateway", format!("{}: {why}\n", target.host))
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = client.write_all(reply.as_bytes()).await;
            return Err(reason.is_policy().then(|| Denial {
                host: target.host.clone(),
                port: target.port,
                requester: rules.requester.clone(),
                reason,
            }));
        }
    };
    relay(client, upstream, &target, &rest)
        .await
        .map_err(|_| None)
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

/// Checks `host:port`, resolves it once and connects to the first
/// admitted address; on refusal, why (and the message for the client).
async fn open(rules: &Rules, host: &str, port: u16) -> Result<TcpStream, (Reason, String)> {
    rules
        .admits(host, port)
        .map_err(|reason| (reason, reason.as_str().replace('_', " ")))?;
    let refused = |r: crate::destination::Refusal| (Reason::from(r.reason), r.to_string());
    rules.policy.check_host(host).map_err(refused)?;
    let literal = host.parse::<IpAddr>().ok();
    let addrs: Vec<IpAddr> = match literal {
        Some(ip) => vec![ip],
        None => rules
            .lookup
            .lookup(host)
            .await
            .map_err(|_| (Reason::Unresolved, "did not resolve".to_owned()))?,
    };
    let named = literal.is_none().then_some(host);
    let mut last = (Reason::Unresolved, "did not resolve".to_owned());
    for ip in addrs {
        if let Err(refusal) = rules.policy.check_addr(named, ip, Some(port)) {
            last = refused(refusal);
            continue;
        }
        // Connect to the address that was checked, never re-resolve.
        let addr = SocketAddr::new(ip, port);
        return match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => Err((Reason::Unreachable, e.to_string())),
            Err(_) => Err((Reason::Unreachable, "connection timed out".to_owned())),
        };
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

    fn serve_once(listener: std::net::TcpListener) {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).unwrap();
            assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("GET /path HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        });
    }

    fn denials() -> (Observer, Arc<std::sync::Mutex<Vec<Denial>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let observer: Observer = Arc::new(move |d| sink.lock().unwrap().push(d));
        (observer, seen)
    }

    #[test]
    fn a_granted_host_is_reached_and_another_is_blocked() {
        let (listener, port) = server();
        serve_once(listener);
        let hosts = Arc::new(HostPolicy::new(Mode::Public, Vec::new()));
        let policy =
            DestinationPolicy::default().with_grant(PrivateGrant::parse("127.0.0.1").unwrap());
        let (observer, seen) = denials();
        let rules = Rules::new(hosts, policy, "plugin:demo")
            .granted(vec![Allowed::parse("127.0.0.1")])
            .observed(observer);
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
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].host, "example.com");
        assert_eq!(seen[0].port, 443);
        assert_eq!(seen[0].requester, "plugin:demo");
        assert_eq!(seen[0].reason, Reason::NotGranted);
    }

    #[test]
    fn a_granted_name_resolving_to_a_private_address_is_refused() {
        let hosts = Arc::new(HostPolicy::new(Mode::Public, Vec::new()));
        let rules = Rules::new(hosts, DestinationPolicy::default(), "terminal")
            .granted(vec![Allowed::parse("localhost")]);
        let proxy = Proxy::start(rules).unwrap();
        let refused = ask(&proxy, "CONNECT localhost:443 HTTP/1.1\r\n\r\n");
        assert!(
            refused.contains("Blocked: localhost is blocked by policy"),
            "{refused}"
        );
    }

    #[test]
    fn an_allowlist_session_refuses_other_hosts_and_follows_session_changes() {
        let hosts = Arc::new(HostPolicy::new(
            Mode::Allowlist,
            vec![Allowed::parse("api.github.com")],
        ));
        let (observer, seen) = denials();
        let rules = Rules::new(Arc::clone(&hosts), DestinationPolicy::default(), "terminal")
            .observed(observer);
        let proxy = Proxy::start(rules).unwrap();
        let refused = ask(&proxy, "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        hosts.set_mode(Mode::None);
        let refused = ask(&proxy, "CONNECT api.github.com:443 HTTP/1.1\r\n\r\n");
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let reasons: Vec<Reason> = seen.lock().unwrap().iter().map(|d| d.reason).collect();
        assert_eq!(reasons, [Reason::NotAllowlisted, Reason::ModeNone]);
    }

    /// Answers a public-looking address first and the metadata address on
    /// every later lookup, like a rebinding DNS server.
    struct Rebinding {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Lookup for Rebinding {
        fn lookup(&self, _host: &str) -> crate::resolve::LookupFuture {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let ip: IpAddr = if call == 0 {
                "127.0.0.1".parse().unwrap()
            } else {
                "169.254.169.254".parse().unwrap()
            };
            Box::pin(async move { Ok(vec![ip]) })
        }
    }

    /// Scenario: DNS rebinding. The first answer passes the check (the
    /// test's fake-IP range makes loopback stand for a public host); the
    /// proxy resolves once and connects to exactly that address, so a
    /// second, rebound answer is never used.
    #[test]
    fn the_proxy_connects_only_to_the_address_it_checked() {
        let (listener, port) = server();
        serve_once(listener);
        let lookup = Arc::new(Rebinding {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let hosts = Arc::new(HostPolicy::new(Mode::Public, Vec::new()));
        let policy = DestinationPolicy::new(false, &["127.0.0.1/32".to_owned()]).unwrap();
        let rules = Rules::new(hosts, policy, "terminal").with_lookup(lookup.clone());
        let proxy = Proxy::start(rules).unwrap();
        let answer = ask(
            &proxy,
            &format!("GET http://rebind.example:{port}/path HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(answer.contains("hello"), "{answer}");
        assert_eq!(lookup.calls.load(Ordering::SeqCst), 1);
        // The rebound answer is the metadata endpoint: refused, not reached.
        let again = ask(
            &proxy,
            &format!("GET http://rebind.example:{port}/path HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(again.starts_with("HTTP/1.1 403"), "{again}");
    }
}
