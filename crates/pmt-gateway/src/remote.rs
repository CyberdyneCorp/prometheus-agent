//! The remote attachment listener (spec: messaging-gateway, Remote client
//! attachment): with `daemon.remote.enabled = true` (default false), desktop
//! and TUI clients on other machines attach on `daemon.remote.listen`
//! (default `0.0.0.0:9121`) over TLS with a paired device token
//! (`Authorization: Bearer`, `prometheus daemon pair-device`) and then speak
//! the same client protocol as local clients. A missing, bad or revoked
//! token closes with 4401, a profile outside the device's binding
//! (`?profiles=a,b`) with 4403; revoking a device closes its open sockets
//! with 4401. TLS is mandatory: a configured certificate, else a
//! self-signed one under `<home>/run/` whose fingerprint the daemon logs
//! for pinning.
//!
//! The path `/adapters` is where a forwarding adapter host (`daemon run
//! --adapters-only --upstream`) attaches instead of a client: it vouches
//! for the senders it forwards, so only an `admin` device may (4403
//! otherwise), and it speaks the relay connector's wire
//! ([`crate::messaging::Gateway::serve_forwarder`]).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::Server;
use crate::connection::Ends;
use crate::devices::{DeviceRole, Devices, Verified};
use crate::tickets::Grant;
use futures_util::SinkExt as _;
use pmt_config::daemon_remote::RemoteConfig;
use pmt_core::error::{Error, codes};
use pmt_rpc::codes::CloseCode;
use pmt_rpc::control::Purpose;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_util::sync::CancellationToken;

/// Where a forwarding adapter host attaches.
pub const FORWARDER_PATH: &str = "/adapters";

/// The health component of the listener.
pub const COMPONENT: &str = "listener:remote";
/// How long a TLS and WebSocket handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A running remote listener.
#[derive(Debug)]
pub struct RemoteListener {
    /// The bound address.
    pub addr: SocketAddr,
    /// The SHA-256 public-key fingerprint clients pin.
    pub fingerprint: String,
    stop: CancellationToken,
}

impl Drop for RemoteListener {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Binds the remote listener when `config.enabled`; it serves until the
/// returned handle drops or the daemon closes.
///
/// # Errors
///
/// `config.invalid` for a bad address or certificate, `net.listen` when
/// the address cannot be bound.
pub async fn start(
    server: &Arc<Server>,
    devices: Arc<Devices>,
    config: &RemoteConfig,
    home: &Path,
) -> Result<Option<RemoteListener>, Error> {
    if !config.enabled {
        return Ok(None);
    }
    let addr = config
        .validate()
        .map_err(|message| Error::new(&codes::CONFIG_INVALID, message))?;
    let (cert, key) = match (&config.tls.cert, &config.tls.key) {
        (Some(cert), Some(key)) => (cert.clone(), key.clone()),
        _ => crate::cert::self_signed(home)?,
    };
    let tls = crate::cert::acceptor(&cert, &key, "daemon.remote.tls")?;
    let fingerprint = crate::cert::fingerprint(&cert, "daemon.remote.tls")?;
    let health = pmt_obs::health::global();
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        let reason = pmt_net::listen::bind_failure(addr, &e);
        health.report(None, COMPONENT, pmt_obs::health::State::Down, &reason);
        Error::new(&codes::NET_LISTEN, format!("daemon.remote: {reason}"))
    })?;
    let addr = listener.local_addr().unwrap_or(addr);
    health.report(
        None,
        COMPONENT,
        pmt_obs::health::State::Ok,
        &format!("listening on {addr}"),
    );
    tracing::info!(%addr, %fingerprint, "remote client attachment listening");
    let stop = CancellationToken::new();
    let serving = Serving {
        server: Arc::clone(server),
        devices,
        stop: stop.clone(),
    };
    pmt_core::spawn::spawn(serving.accept_loop(listener, tls));
    Ok(Some(RemoteListener {
        addr,
        fingerprint,
        stop,
    }))
}

#[derive(Clone)]
struct Serving {
    server: Arc<Server>,
    devices: Arc<Devices>,
    stop: CancellationToken,
}

impl Serving {
    async fn accept_loop(self, listener: TcpListener, tls: tokio_rustls::TlsAcceptor) {
        let closing = self.server.closing();
        loop {
            let accepted = tokio::select! {
                () = self.stop.cancelled() => return,
                () = closing.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _peer)) = accepted else {
                continue;
            };
            let this = self.clone();
            let tls = tls.clone();
            pmt_core::spawn::spawn(async move {
                if let Ok(Ok(stream)) =
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)).await
                {
                    this.connection(stream).await;
                }
            });
        }
    }

    // tungstenite's handshake callback returns its own `ErrorResponse`.
    #[allow(clippy::result_large_err)]
    async fn connection<S: crate::connection::Transport>(&self, stream: S) {
        let mut admission = None;
        let mut forwarder = false;
        let callback = |request: &Request, response: Response| {
            forwarder = request.uri().path() == FORWARDER_PATH;
            Ok::<_, ErrorResponse>(self.handshake(request, response, &mut admission))
        };
        let Ok(Ok(mut socket)) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::accept_hdr_async(stream, callback),
        )
        .await
        else {
            return;
        };
        let admission = admission.unwrap_or(Err(CloseCode::BadCredential));
        let admission = match admission {
            Ok((verified, _)) if forwarder && verified.role != DeviceRole::Admin => {
                Err(CloseCode::NotAllowed)
            }
            other => other,
        };
        match admission {
            Ok((_, profiles)) if forwarder => self.forwarder(socket, profiles).await,
            Ok((verified, profiles)) => {
                let grant = Grant {
                    profiles,
                    purpose: Purpose::Interactive,
                    pid: None,
                    observer_only: verified.role == DeviceRole::Observer,
                    admin: verified.role == DeviceRole::Admin,
                };
                let ends = Ends {
                    stop: self.stop.clone(),
                    revoked: verified.revoked,
                };
                crate::connection::serve_upgraded(Arc::clone(&self.server), socket, grant, ends)
                    .await;
            }
            Err(code) => close(&mut socket, code).await,
        }
    }

    /// Serves an attached forwarding adapter host until it disconnects. A
    /// refused one (another host attached, a dialed relay) is dropped and
    /// retries; without a messaging gateway it is closed as disabled.
    async fn forwarder<S: crate::connection::Transport>(
        &self,
        mut socket: tokio_tungstenite::WebSocketStream<S>,
        profiles: Vec<String>,
    ) {
        let Some(gateway) = self.server.messaging().cloned() else {
            close(&mut socket, CloseCode::EndpointDisabled).await;
            return;
        };
        if let Err(why) = gateway.serve_forwarder(socket, profiles).await {
            tracing::warn!("forwarding adapter host: {why}");
        }
    }

    /// Echoes the protocol subprotocol and decides the admission; a
    /// refused client still completes the handshake so it sees the code.
    fn handshake(
        &self,
        request: &Request,
        mut response: Response,
        admission: &mut Option<Result<(Verified, Vec<String>), CloseCode>>,
    ) -> Response {
        let offered = request
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .find(|p| p.starts_with("prometheus-rpc.v"))
            .map(str::to_owned);
        if let Some(value) = offered.and_then(|p| HeaderValue::from_str(&p).ok()) {
            response
                .headers_mut()
                .insert("sec-websocket-protocol", value);
        }
        *admission = Some(self.admit(request));
        response
    }

    fn admit(&self, request: &Request) -> Result<(Verified, Vec<String>), CloseCode> {
        let bearer = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim());
        let verified = bearer
            .and_then(|token| self.devices.verify(token))
            .ok_or(CloseCode::BadCredential)?;
        let served = self.server.profiles();
        let bound: Vec<String> = served
            .into_iter()
            .filter(|p| verified.profiles.is_empty() || verified.profiles.contains(p))
            .collect();
        let asked: Option<Vec<String>> = request.uri().query().and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "profiles")
                .map(|(_, v)| v.split(',').map(str::to_owned).collect())
        });
        let profiles = match asked {
            Some(asked) if asked.iter().any(|p| !bound.contains(p)) => {
                return Err(CloseCode::NotAllowed);
            }
            Some(asked) => asked,
            None => bound,
        };
        Ok((verified, profiles))
    }
}

async fn close<S: crate::connection::Transport>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    code: CloseCode,
) {
    let close = Message::Close(Some(CloseFrame {
        code: code.code().into(),
        reason: code.reason().into(),
    }));
    let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(close)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
}
