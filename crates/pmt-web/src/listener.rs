//! The dashboard's socket (spec: web-dashboard, Dashboard command and
//! listener defaults; Non-loopback binds fail closed; Listener lifecycle
//! within the daemon): the fail-closed bind checks, TLS from
//! `dashboard.tls`, and the accept loop serving HTTP/1.1 with upgrades
//! until the listener stops.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use pmt_config::daemon_dashboard::{AuthMode, DashboardConfig, PASSWORD_HASH_SECRET};
use pmt_core::error::{Error, codes};
use pmt_gateway::connection::Transport;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::Instrument as _;

use crate::Shared;

/// Connections served at once; the next waits in the accept queue.
const MAX_CONNECTIONS: usize = 256;
/// How long a client may take to send a request's headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a TLS handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The health component of the listener.
pub(crate) const COMPONENT: &str = "listener:dashboard";

/// The first thing a non-loopback bind lacks: configuration first, then
/// the provider's secret (read from the launch profile's store).
pub(crate) fn check_remote(
    config: &DashboardConfig,
    secret_set: impl Fn(&str) -> bool,
) -> Result<(), Error> {
    let missing = config.missing_for_remote().or_else(|| {
        (!config.loopback()
            && config.auth.mode == AuthMode::Password
            && !secret_set(PASSWORD_HASH_SECRET))
        .then(|| format!("the {PASSWORD_HASH_SECRET} secret"))
    });
    match missing {
        Some(missing) => Err(Error::new(
            &codes::DASHBOARD_BIND_REFUSED,
            config.refusal(&missing),
        )),
        None => Ok(()),
    }
}

/// The TLS acceptor of `dashboard.tls`, when configured.
pub(crate) fn tls(config: &DashboardConfig) -> Result<Option<TlsAcceptor>, Error> {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let (Some(cert), Some(key)) = (&config.tls.cert, &config.tls.key) else {
        return Ok(None);
    };
    let bad = |what: &str, path: &std::path::Path, e: &dyn std::fmt::Display| {
        Error::new(
            &codes::CONFIG_INVALID,
            format!("dashboard.tls.{what}: cannot load {}: {e}", path.display()),
        )
    };
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| bad("cert", cert, &e))?;
    let key_der = PrivateKeyDer::from_pem_file(key).map_err(|e| bad("key", key, &e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_no_client_auth().with_single_cert(chain, key_der))
        .map_err(|e| bad("cert", cert, &e))?;
    Ok(Some(TlsAcceptor::from(Arc::new(server))))
}

/// Binds the listener: a port another process holds exits 78 naming it.
pub(crate) async fn bind(config: &DashboardConfig) -> Result<TcpListener, Error> {
    let host = config.host.trim_start_matches('[').trim_end_matches(']');
    let host = if host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1"
    } else {
        host
    };
    let addr = format!("{host}:{}", config.port)
        .parse::<SocketAddr>()
        .or_else(|_| format!("[{host}]:{}", config.port).parse())
        .map_err(|_| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!("dashboard.host: '{}' is not an IP address", config.host),
            )
        })?;
    let health = pmt_obs::health::global();
    match TcpListener::bind(addr).await {
        Ok(listener) => {
            let at = listener.local_addr().unwrap_or(addr);
            health.report(
                None,
                COMPONENT,
                pmt_obs::health::State::Ok,
                &format!("listening on {at}"),
            );
            Ok(listener)
        }
        Err(e) => {
            let reason = pmt_net::listen::bind_failure(addr, &e);
            health.report(None, COMPONENT, pmt_obs::health::State::Down, &reason);
            let err = if e.kind() == std::io::ErrorKind::AddrInUse {
                Error::new(
                    &codes::DASHBOARD_PORT_IN_USE,
                    format!(
                        "dashboard port {} is in use; set dashboard.port or pass --port",
                        config.port
                    ),
                )
            } else {
                Error::new(&codes::NET_LISTEN, format!("dashboard: {reason}"))
            };
            Err(err.with_source(e))
        }
    }
}

/// Serves `listener` until the dashboard stops.
pub(crate) fn serve(listener: TcpListener, tls: Option<TlsAcceptor>, shared: Arc<Shared>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    pmt_core::spawn::spawn(
        async move {
            loop {
                let accepted = tokio::select! {
                    () = shared.stop.cancelled() => break,
                    accepted = accept(&listener, &slots) => accepted,
                };
                let Some((stream, peer, slot)) = accepted else {
                    continue;
                };
                let shared = Arc::clone(&shared);
                let tls = tls.clone();
                pmt_core::spawn::spawn(async move {
                    let _slot = slot;
                    match tls {
                        None => connection(stream, peer, shared).await,
                        Some(tls) => {
                            let handshake =
                                tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream));
                            if let Ok(Ok(stream)) = handshake.await {
                                connection(stream, peer, shared).await;
                            }
                        }
                    }
                });
            }
            pmt_obs::health::global().report(
                None,
                COMPONENT,
                pmt_obs::health::State::Ok,
                "stopped",
            );
        }
        .in_current_span(),
    );
}

async fn accept(
    listener: &TcpListener,
    slots: &Arc<tokio::sync::Semaphore>,
) -> Option<(
    tokio::net::TcpStream,
    SocketAddr,
    tokio::sync::OwnedSemaphorePermit,
)> {
    let slot = Arc::clone(slots).acquire_owned().await.ok()?;
    let (stream, peer) = listener.accept().await.ok()?;
    Some((stream, peer, slot))
}

/// One HTTP connection, ended when the listener stops.
async fn connection<S: Transport>(stream: S, peer: SocketAddr, shared: Arc<Shared>) {
    let stop = shared.stop.clone();
    let service = service_fn(move |req| {
        let shared = Arc::clone(&shared);
        async move { Ok::<_, std::convert::Infallible>(crate::routes::answer(&shared, req, peer).await) }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::select! {
        result = conn => {
            if let Err(err) = result {
                tracing::debug!("dashboard connection ended: {err}");
            }
        }
        () = stop.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_dashboard::{AuthMode, DashboardConfig};

    use super::check_remote;

    /// Scenario: LAN bind without auth.
    #[test]
    fn a_lan_bind_without_auth_or_its_secret_is_refused() {
        let lan = DashboardConfig::default().with_overrides(Some("0.0.0.0"), None);
        let err = check_remote(&lan, |_| true).unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message().contains("dashboard.auth.mode"),
            "{}",
            err.message()
        );
        let mut password = lan;
        password.auth.mode = AuthMode::Password;
        password.trust_proxy_tls = true;
        let err = check_remote(&password, |_| false).unwrap_err();
        assert!(
            err.message().contains("DASHBOARD_PASSWORD_HASH"),
            "{}",
            err.message()
        );
        assert!(check_remote(&password, |_| true).is_ok());
        assert!(check_remote(&DashboardConfig::default(), |_| false).is_ok());
    }
}
