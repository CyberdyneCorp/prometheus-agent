//! Dialing `remote` and `ssh` connections: the page's dials go through a
//! loopback [`Forwarder`] per connection, opened when an agent on that
//! connection is first dialed (never on enumeration), and the shell's own
//! requests (Test, pairing, the roster, Update all) dial the same way.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use url::Url;

use super::Kind;
use super::forward::{Forwarder, Head, Opened, Upstream};
use super::service::{Latch, Shared};
use super::ssh::{Ssh, SshFailure};
use super::tls::{self, BoxStream, DialFailure};
use super::web::{self, Socket, Upgrade};
use super::{Connection, SshTarget};
use crate::dial::{Dial, DialPurpose};
use crate::endpoint;
use crate::shell::Shell;

/// The ticket subprotocol's prefix.
const TICKET: &str = "prometheus-ticket.";

/// A connection's running forwarder.
pub(crate) struct Live {
    forwarder: Forwarder,
    /// The remote daemon's loopback port (`ssh`), updated when it moves.
    port: Option<Arc<AtomicU16>>,
}

fn protocol() -> String {
    format!("prometheus-rpc.v{}", pmt_core::protocol::RPC_PROTOCOL.major)
}

/// `path` below a remote connection's base path.
pub(crate) fn api(url: &Url, path: &str) -> String {
    format!("{}{path}", url.path().trim_end_matches('/'))
}

/// The authority (`host[:port]`) a remote server expects in `Host`.
pub(crate) fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// A `remote` connection's forwarded dials: the shell's one-use ticket is
/// swapped for the device credential, the path for the dashboard's
/// `/api/rpc`, and the server is dialed with the pin.
struct RemoteUpstream {
    id: String,
    url: Url,
    shared: Arc<Shared>,
}

impl Upstream for RemoteUpstream {
    fn open(&self, mut head: Head) -> Opened<'_> {
        Box::pin(async move {
            if head.path != endpoint::RPC_PATH {
                return Err(format!("unknown gateway path {}", head.path));
            }
            let protocols = head.protocols();
            let ticket = protocols.iter().find_map(|p| p.strip_prefix(TICKET));
            if !ticket.is_some_and(|t| self.shared.redeem(&self.id, t)) {
                return Err("a forwarded dial needs a ticket of this shell".to_owned());
            }
            let token = remote_token(&self.shared, &self.id)?;
            let stream = dial_remote(&self.shared, &self.id, &self.url).await?;
            head.path = api(&self.url, "/api/rpc");
            head.set("Host", Some(authority(&self.url)));
            head.set("Origin", None);
            head.set("Authorization", Some(format!("Bearer {token}")));
            let kept: Vec<String> = protocols
                .into_iter()
                .filter(|p| !p.starts_with(TICKET))
                .collect();
            head.set("Sec-WebSocket-Protocol", Some(kept.join(", ")));
            Ok((head, stream))
        })
    }
}

/// An `ssh` connection's forwarded dials: `ssh -W` to the remote daemon's
/// loopback port; the remote daemon checks its own ticket.
struct SshUpstream {
    ssh: Ssh,
    target: SshTarget,
    port: Arc<AtomicU16>,
}

impl Upstream for SshUpstream {
    fn open(&self, head: Head) -> Opened<'_> {
        Box::pin(async move {
            if head.path != endpoint::RPC_PATH {
                return Err(format!("unknown gateway path {}", head.path));
            }
            let stream = self
                .ssh
                .pipe(&self.target, self.port.load(Ordering::SeqCst))?;
            Ok((head, stream))
        })
    }
}

fn remote_token(shared: &Shared, id: &str) -> Result<String, String> {
    shared
        .credentials
        .as_ref()
        .ok_or("this shell keeps no credentials")?
        .token(id)?
        .ok_or_else(|| "this connection is not paired yet".to_owned())
}

/// Dials a `remote` connection's server with its pin; a changed key is
/// latched.
pub(crate) async fn dial_remote(shared: &Shared, id: &str, url: &Url) -> Result<BoxStream, String> {
    if let Some(latch) = shared.latched(id) {
        return Err(latch.message().to_owned());
    }
    let pin = shared.pins.get(id);
    tls::dial(url, pin.as_deref())
        .await
        .map_err(|failure| dial_error(shared, id, &failure))
}

/// A dial failure as an error; a changed certificate is latched.
pub(crate) fn dial_error(shared: &Shared, id: &str, failure: &DialFailure) -> String {
    if let DialFailure::PinMismatch { pinned, presented } = failure {
        shared.latch(
            id,
            Latch::Certificate {
                pinned: pinned.clone(),
                presented: presented.clone(),
                message: failure.to_string(),
            },
        );
    }
    failure.to_string()
}

/// An `ssh` failure as a dial error; a changed host key is latched.
pub(crate) fn ssh_error(shared: &Shared, id: &str, failure: &SshFailure) -> String {
    if let SshFailure::HostKeyChanged { fingerprint } = failure {
        shared.latch(
            id,
            Latch::HostKey {
                fingerprint: fingerprint.clone(),
                message: failure.to_string(),
            },
        );
    }
    failure.to_string()
}

fn remote_url(connection: &Connection) -> Result<Url, String> {
    let raw = connection.url.as_deref().ok_or("no URL")?;
    Url::parse(raw).map_err(|e| e.to_string())
}

fn ssh_target(connection: &Connection) -> Result<&SshTarget, String> {
    connection
        .ssh
        .as_ref()
        .ok_or_else(|| "no ssh target".to_owned())
}

fn loopback_port(rpc_url: &str) -> Result<u16, String> {
    let url = Url::parse(rpc_url).map_err(|e| e.to_string())?;
    match url.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => {}
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => {}
        _ => return Err("the remote daemon's endpoint is not on its loopback".to_owned()),
    }
    url.port()
        .ok_or_else(|| "the remote daemon's endpoint has no port".to_owned())
}

impl Shell {
    /// A dial of `window` to `profile` on connection `id` (not `local`).
    pub(crate) async fn dial_connection(
        &self,
        window: &str,
        id: &str,
        profile: &str,
        path: &str,
        purpose: DialPurpose,
    ) -> Result<Dial, String> {
        if path != endpoint::RPC_PATH {
            return Err(format!("unknown gateway path {path}"));
        }
        let shared = &self.connections.shared;
        if let Some(latch) = shared.latched(id) {
            return Err(latch.message().to_owned());
        }
        let connection = self.connections.get(id)?;
        let reserved = self
            .dials
            .reserve(window, path, purpose, Instant::now())
            .map_err(|e| e.to_string())?;
        let issued = self.forwarded_ticket(&connection, profile).await;
        let (port, ticket) = match issued {
            Ok(found) => found,
            Err(e) => {
                let _ = self.dials.settle(window, &reserved);
                return Err(e);
            }
        };
        Ok(Dial {
            dial: reserved,
            url: format!("ws://127.0.0.1:{port}{}", endpoint::RPC_PATH),
            protocols: vec![protocol(), format!("{TICKET}{ticket}")],
        })
    }

    /// The connection's forwarder port and a ticket for one dial through it.
    async fn forwarded_ticket(
        &self,
        connection: &Connection,
        profile: &str,
    ) -> Result<(u16, String), String> {
        let shared = &self.connections.shared;
        let mut live = self.connections.live.lock().await;
        if !live.contains_key(&connection.id) {
            let started = self.start_forwarder(connection).await?;
            live.insert(connection.id.clone(), started);
        }
        let entry = live.get(&connection.id).ok_or("no forwarder")?;
        let port = entry.forwarder.port;
        if connection.kind == Kind::Remote {
            return Ok((port, shared.issue(&connection.id)?));
        }
        let target = ssh_target(connection)?;
        let ticket = self
            .connections
            .ssh()?
            .ticket(target, Some(profile))
            .await
            .map_err(|f| ssh_error(shared, &connection.id, &f))?;
        // A daemon restarted on another port is followed.
        if let (Some(moved), Ok(now)) = (&entry.port, loopback_port(&ticket.rpc_url)) {
            moved.store(now, Ordering::SeqCst);
        }
        Ok((port, ticket.ticket))
    }

    async fn start_forwarder(&self, connection: &Connection) -> Result<Live, String> {
        let shared = &self.connections.shared;
        let (upstream, port): (Arc<dyn Upstream>, _) = match connection.kind {
            Kind::Remote => {
                let upstream = RemoteUpstream {
                    id: connection.id.clone(),
                    url: remote_url(connection)?,
                    shared: Arc::clone(shared),
                };
                (Arc::new(upstream), None)
            }
            Kind::Ssh => {
                let (ssh, target, port) = self.ensure_ssh(connection).await?;
                let port = Arc::new(AtomicU16::new(port));
                let upstream = SshUpstream {
                    ssh,
                    target,
                    port: Arc::clone(&port),
                };
                (Arc::new(upstream), Some(port))
            }
            Kind::Local => return Err("the local connection is dialed directly".to_owned()),
        };
        let forwarder = Forwarder::start(upstream)
            .await
            .map_err(|e| format!("cannot open a loopback port: {e}"))?;
        Ok(Live { forwarder, port })
    }

    /// Runs `daemon ensure` on an `ssh` connection's host: its daemon's
    /// loopback port there.
    pub(crate) async fn ensure_ssh(
        &self,
        connection: &Connection,
    ) -> Result<(Ssh, SshTarget, u16), String> {
        let ssh = self.connections.ssh()?.clone();
        let target = ssh_target(connection)?.clone();
        let report = ssh
            .ensure(&target)
            .await
            .map_err(|f| ssh_error(&self.connections.shared, &connection.id, &f))?;
        let port = loopback_port(&report.rpc_url)?;
        Ok((ssh, target, port))
    }

    /// A client protocol socket of the shell's own on connection `id`.
    pub(crate) async fn socket(&self, connection: &Connection) -> Result<Socket, String> {
        let shared = &self.connections.shared;
        let protocols = vec![protocol()];
        match connection.kind {
            Kind::Remote => {
                let url = remote_url(connection)?;
                let stream = dial_remote(shared, &connection.id, &url).await?;
                let token = remote_token(shared, &connection.id)?;
                let target = format!("ws://{}{}", authority(&url), api(&url, "/api/rpc"));
                let upgrade = Upgrade {
                    url: &target,
                    protocols: &protocols,
                    bearer: Some(&token),
                };
                Socket::open(stream, &upgrade).await
            }
            Kind::Ssh => {
                let (ssh, target, port) = self.ensure_ssh(connection).await?;
                let ticket = ssh
                    .ticket(&target, None)
                    .await
                    .map_err(|f| ssh_error(shared, &connection.id, &f))?;
                let port = loopback_port(&ticket.rpc_url).unwrap_or(port);
                let stream = ssh.pipe(&target, port)?;
                let url = format!("ws://127.0.0.1:{port}{}", endpoint::RPC_PATH);
                let protocols = vec![protocol(), format!("{TICKET}{}", ticket.ticket)];
                let upgrade = Upgrade {
                    url: &url,
                    protocols: &protocols,
                    bearer: None,
                };
                Socket::open(stream, &upgrade).await
            }
            Kind::Local => Err("the local connection is not dialed by the shell".to_owned()),
        }
    }

    /// One HTTP request to a `remote` connection's dashboard listener.
    pub(crate) async fn remote_request(
        &self,
        connection: &Connection,
        method: &str,
        path: &str,
        body: Option<&Value>,
        bearer: bool,
    ) -> Result<web::Reply, String> {
        let shared = &self.connections.shared;
        let url = remote_url(connection)?;
        // Reachability first: an offline server reads as unreachable even
        // before it is paired.
        let stream = dial_remote(shared, &connection.id, &url).await?;
        let token = if bearer {
            Some(remote_token(shared, &connection.id)?)
        } else {
            None
        };
        web::request(
            stream,
            &authority(&url),
            method,
            &api(&url, path),
            token.as_deref(),
            body,
        )
        .await
    }

    /// Exchanges a pairing code on a `remote` connection's server for the
    /// device credential.
    pub(crate) async fn exchange_code(
        &self,
        connection: &Connection,
        code: &str,
    ) -> Result<String, String> {
        let body = json!({"code": code});
        let reply = self
            .remote_request(connection, "POST", "/api/devices/pair", Some(&body), false)
            .await?;
        let value = reply.json();
        match (reply.status, value["token"].as_str()) {
            (200, Some(token)) => Ok(token.to_owned()),
            _ => Err(value["message"]
                .as_str()
                .or_else(|| value["error"].as_str())
                .map_or_else(
                    || format!("the server refused the pairing code ({})", reply.status),
                    str::to_owned,
                )),
        }
    }
}
