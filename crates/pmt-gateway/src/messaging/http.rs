//! The shared HTTP listener of webhook-style adapters (spec:
//! platform-adapters, Shared HTTP listener for webhook-style platforms):
//! adapters with `binds_http` never open sockets; the daemon serves them
//! all on one listener (`daemon.toml gateway.http.listen`, default
//! `127.0.0.1:8644`) under `/p/<profile>/<platform>/...`. A request the
//! adapter cannot verify (signature or shared secret) is answered 401 and
//! admits nothing. The listener starts with the first such adapter.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, Weak};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{Adapter, HttpRequest, HttpResponse};

/// Where the listener binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpOptions {
    /// `gateway.http.listen`.
    pub listen: SocketAddr,
    /// `gateway.http.public`: a non-loopback address is allowed.
    pub public: bool,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8644)),
            public: false,
        }
    }
}

/// The listener, started once.
#[derive(Debug, Default)]
pub(crate) struct Listener {
    bound: OnceLock<Result<SocketAddr, String>>,
}

impl Listener {
    /// The bound address, when the listener runs.
    pub(crate) fn addr(&self) -> Option<SocketAddr> {
        self.bound.get().and_then(|r| r.as_ref().ok().copied())
    }
}

/// Starts the listener unless it runs; the bound address, or why it
/// cannot listen.
pub(crate) fn ensure(shared: &Arc<Shared>) -> Result<SocketAddr, String> {
    shared.http.bound.get_or_init(|| start(shared)).clone()
}

fn start(shared: &Arc<Shared>) -> Result<SocketAddr, String> {
    let options = shared.options.http;
    if !pmt_net::listen::is_loopback(options.listen.ip()) {
        if !options.public {
            return Err(format!(
                "gateway.http.listen {} is not a loopback address; set gateway.http.public = true in daemon.toml to expose it",
                options.listen
            ));
        }
        tracing::warn!(
            "the webhook listener on {} is reachable from other hosts (gateway.http.public = true)",
            options.listen
        );
    }
    let bound = std::net::TcpListener::bind(options.listen)
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .and_then(tokio::net::TcpListener::from_std)
        .and_then(|l| l.local_addr().map(|addr| (l, addr)));
    let (listener, addr) = match bound {
        Ok(bound) => bound,
        Err(e) => {
            let reason = pmt_net::listen::bind_failure(options.listen, &e);
            // Only this listener is down: every other listener, adapter
            // and profile keeps running (spec: distribution-and-update,
            // Multiple profiles in one container).
            listener_health(pmt_obs::health::State::Down, &reason);
            return Err(reason);
        }
    };
    listener_health(pmt_obs::health::State::Ok, &format!("listening on {addr}"));
    tracing::info!("webhook listener on http://{addr}/p/<profile>/<platform>/");
    let weak = Arc::downgrade(shared);
    let stop = shared.stop.clone();
    pmt_core::spawn::spawn(
        async move {
            loop {
                let accepted = tokio::select! {
                    () = stop.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { continue };
                let weak = Weak::clone(&weak);
                pmt_core::spawn::spawn(async move {
                    let service = service_fn(move |req| serve(Weak::clone(&weak), req));
                    let conn = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    if let Err(err) = conn.await {
                        tracing::debug!("webhook connection ended: {err}");
                    }
                });
            }
        }
        .in_current_span(),
    );
    Ok(addr)
}

/// The health component of the shared webhook listener.
pub(crate) const LISTENER_COMPONENT: &str = "listener:gateway.http";

fn listener_health(state: pmt_obs::health::State, reason: &str) {
    pmt_obs::health::global().report(None, LISTENER_COMPONENT, state, reason);
}

async fn serve(
    shared: Weak<Shared>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let answer = match shared.upgrade() {
        Some(shared) => route(&shared, request).await,
        None => HttpResponse::status(503),
    };
    Ok(respond(answer))
}

/// The hyper response of an adapter's answer.
pub(crate) fn respond(answer: HttpResponse) -> Response<Full<Bytes>> {
    let mut response = Response::builder().status(answer.status);
    if let Some(content_type) = &answer.content_type {
        response = response.header("content-type", content_type);
    }
    for (name, value) in &answer.headers {
        response = response.header(name, value);
    }
    response
        .body(Full::new(Bytes::from(answer.body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// `/p/<profile>/<platform>/<rest>` to its adapter.
async fn route(shared: &Shared, request: Request<Incoming>) -> HttpResponse {
    let path = request.uri().path().to_owned();
    let mut segments = path.trim_start_matches('/').splitn(4, '/');
    let (Some("p"), Some(profile), Some(platform)) =
        (segments.next(), segments.next(), segments.next())
    else {
        return HttpResponse::status(404);
    };
    let rest = format!("/{}", segments.next().unwrap_or_default());
    let Some(slot) = shared
        .adapters
        .ready(profile, platform)
        .filter(|s| s.descriptor.binds_http)
    else {
        return HttpResponse::status(404);
    };
    deliver(slot.adapter.as_ref(), rest, request).await
}

/// Hands a request (its path below the adapter's prefix being `rest`) to
/// `adapter`: answered 401 unless the adapter verifies it.
pub(crate) async fn deliver(
    adapter: &dyn Adapter,
    rest: String,
    request: Request<Incoming>,
) -> HttpResponse {
    let (parts, body) = request.into_parts();
    let limit = adapter.max_body_bytes();
    // A declared length over the limit is refused before anything is read.
    let declared = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return HttpResponse::status(413);
    }
    let Ok(body) = Limited::new(body, limit).collect().await else {
        return HttpResponse::status(413);
    };
    let incoming = HttpRequest {
        method: parts.method.as_str().to_owned(),
        path: rest,
        query: parts.uri.query().map(str::to_owned),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: body.to_bytes().to_vec(),
    };
    if !adapter.verify_http(&incoming) {
        tracing::warn!(
            platform = adapter.describe().platform,
            "refused an unsigned or wrongly signed webhook request"
        );
        return HttpResponse::status(401);
    }
    adapter.handle_http(&incoming).await
}
