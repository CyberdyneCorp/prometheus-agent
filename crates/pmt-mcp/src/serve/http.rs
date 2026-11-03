//! `prometheus mcp serve --http`: the MCP server over streamable HTTP
//! (spec: plugin-system, MCP server mode).
//!
//! Every request must carry `Authorization: Bearer <token>` (the profile's
//! `MCP_SERVER_TOKEN`); anything else is answered 401 before it reaches the
//! MCP layer. The caller decides the address (loopback unless
//! `mcp_server.allow_remote`); on loopback the MCP layer also refuses
//! requests whose `Host` is not a loopback name (DNS rebinding).

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;

use super::{Host, Serving};

type Service = StreamableHttpService<Serving, LocalSessionManager>;

/// Serves `host` on `listener` until the task is dropped or aborted.
/// `remote`: the address is not loopback, so any `Host` header is
/// accepted (the bearer token is the gate).
///
/// # Errors
///
/// The listener failed.
pub async fn serve(
    host: Arc<dyn Host>,
    listener: TcpListener,
    token: String,
    remote: bool,
) -> Result<(), String> {
    let mut config = StreamableHttpServerConfig::default();
    if remote {
        config = config.disable_allowed_hosts();
    }
    let service: Service = StreamableHttpService::new(
        move || Ok(Serving::new(Arc::clone(&host))),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let token: Arc<str> = token.into();
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let (service, token) = (service.clone(), Arc::clone(&token));
        tokio::spawn(async move {
            let handler = hyper::service::service_fn(move |request: Request<Incoming>| {
                let (service, token) = (service.clone(), Arc::clone(&token));
                async move { Ok::<_, Infallible>(answer(&service, &token, request).await) }
            });
            let connection = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), handler);
            if let Err(err) = connection.await {
                tracing::debug!(target: "mcp", "MCP HTTP connection ended: {err}");
            }
        });
    }
}

async fn answer(
    service: &Service,
    token: &str,
    request: Request<Incoming>,
) -> Response<BoxBody<Bytes, Infallible>> {
    if !authorized(&request, token) {
        let body = Full::new(Bytes::from_static(b"missing or wrong bearer token\n")).boxed();
        let mut response = Response::new(body);
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        response.headers_mut().insert(
            hyper::header::WWW_AUTHENTICATE,
            hyper::header::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    service.handle(request).await
}

/// Whether the request carries `Bearer <token>` (compared in constant
/// time).
fn authorized<B>(request: &Request<B>, token: &str) -> bool {
    let Some(given) = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (given.as_bytes(), token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
