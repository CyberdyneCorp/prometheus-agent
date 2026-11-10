//! The daemon's loopback health listener (spec: observability, Health
//! endpoints; Local metrics scrape endpoint): `GET /healthz` answers 200
//! `{"status":"ok"}` while the runtime runs a probe task within 1 s,
//! `GET /readyz` 200 when every profile's store is open and migrated and
//! the scheduler runs, else 503 naming the failing component kinds, and
//! `GET /metrics` the metrics catalog in Prometheus text format when
//! `observability.metrics_endpoint.enabled` (404 otherwise). No
//! authentication; nothing names a profile, a session or a secret.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_net::listen::{Exposure, bind};
use serde_json::json;
use tracing::Instrument as _;

/// The component kinds readiness requires.
pub const READY_KINDS: &[&str] = &["store", "scheduler"];
/// How long the runtime has to run the liveness probe.
const PROBE_WITHIN: Duration = Duration::from_secs(1);

/// What the listener serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthOptions {
    /// Serve `GET /metrics`.
    pub metrics: bool,
}

/// Binds the loopback listener at `addr` (`observability.health.bind`).
///
/// # Errors
///
/// `net.listen_refused` for a non-loopback address, `net.listen` when it
/// cannot be bound.
pub async fn listen(addr: SocketAddr) -> Result<tokio::net::TcpListener, Error> {
    if !addr.ip().is_loopback() {
        return Err(Error::new(
            &codes::NET_LISTEN_REFUSED,
            format!("observability.health.bind {addr} is not a loopback address"),
        ));
    }
    bind("daemon health", addr, Exposure::default()).await
}

/// Serves `listener` until `stop`.
pub fn serve(listener: tokio::net::TcpListener, options: HealthOptions, stop: CancelToken) {
    tokio::spawn(
        async move {
            loop {
                let accepted = tokio::select! {
                    () = stop.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { continue };
                tokio::spawn(async move {
                    let service = service_fn(move |req| answer(options, req));
                    let conn = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(err) = conn.await {
                        tracing::debug!("health connection ended: {err}");
                    }
                });
            }
        }
        .in_current_span(),
    );
}

async fn answer(
    options: HealthOptions,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    if request.method() != Method::GET {
        return Ok(respond(
            405,
            "text/plain",
            "method not allowed\n".to_owned(),
        ));
    }
    Ok(match request.uri().path() {
        "/healthz" => healthz().await,
        "/readyz" => readyz(),
        "/metrics" if options.metrics => respond(
            200,
            pmt_obs::metrics::text::CONTENT_TYPE,
            pmt_obs::metrics::text::render(&pmt_obs::metrics::global().snapshot()),
        ),
        _ => respond(404, "text/plain", "not found\n".to_owned()),
    })
}

async fn healthz() -> Response<Full<Bytes>> {
    let probe = tokio::spawn(async {});
    match tokio::time::timeout(PROBE_WITHIN, probe).await {
        Ok(Ok(())) => respond(200, "application/json", json!({"status": "ok"}).to_string()),
        _ => respond(
            503,
            "application/json",
            json!({"status": "unresponsive"}).to_string(),
        ),
    }
}

fn readyz() -> Response<Full<Bytes>> {
    let failing = pmt_obs::health::global().failing(READY_KINDS);
    if failing.is_empty() {
        respond(200, "application/json", json!({"status": "ok"}).to_string())
    } else {
        respond(
            503,
            "application/json",
            json!({"status": "not_ready", "failing": failing}).to_string(),
        )
    }
}

fn respond(status: u16, content_type: &str, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
