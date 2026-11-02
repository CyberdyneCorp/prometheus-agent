//! A tiny loopback HTTP server for the mock platforms the conformance kit
//! runs adapters against: each request, body read whole, goes to one
//! handler as an [`HttpRequest`] and its [`HttpResponse`] is sent back.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;

use http_body_util::BodyExt as _;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;

use super::adapter::{HttpRequest, HttpResponse};

/// Serves `handler` on a free loopback port until the runtime ends; the
/// bound address.
///
/// # Errors
///
/// No loopback port can be bound.
pub async fn serve<H, F>(handler: H) -> std::io::Result<SocketAddr>
where
    H: Fn(HttpRequest) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = HttpResponse> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                    let handler = handler.clone();
                    async move {
                        let request = read(request).await;
                        Ok::<_, Infallible>(super::http::respond(handler(request).await))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(addr)
}

async fn read(request: hyper::Request<hyper::body::Incoming>) -> HttpRequest {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();
    HttpRequest {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
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
        body,
    }
}

/// A JSON answer.
#[must_use]
pub fn json(status: u16, body: &Value) -> HttpResponse {
    HttpResponse::json(status, body)
}

/// A raw body answer.
#[must_use]
pub fn bytes(status: u16, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        content_type: Some("application/octet-stream".to_owned()),
        body,
        ..HttpResponse::status(status)
    }
}
