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
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            pmt_core::spawn::spawn(async move {
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

/// One part of a `multipart/form-data` body a mock received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPart {
    /// The field name.
    pub name: String,
    /// The file name, for a file.
    pub file_name: Option<String>,
    /// The content.
    pub bytes: Vec<u8>,
}

impl FormPart {
    /// The content as text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// The parts of a `multipart/form-data` body sent with `content_type`
/// (empty when it is not one).
#[must_use]
pub fn form_parts(content_type: &str, body: &[u8]) -> Vec<FormPart> {
    let Some(boundary) = content_type.split("boundary=").nth(1) else {
        return Vec::new();
    };
    let delimiter = format!("--{}", boundary.trim_matches('"'));
    split_on(body, delimiter.as_bytes())
        .into_iter()
        .filter_map(form_part)
        .collect()
}

fn split_on<'a>(body: &'a [u8], delimiter: &[u8]) -> Vec<&'a [u8]> {
    let mut pieces = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.windows(delimiter.len()).position(|w| w == delimiter) {
        pieces.push(&rest[..at]);
        rest = &rest[at + delimiter.len()..];
    }
    pieces.push(rest);
    pieces
}

fn form_part(piece: &[u8]) -> Option<FormPart> {
    let piece = piece.strip_prefix(b"\r\n")?;
    let end = piece.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&piece[..end]);
    let content = &piece[end + 4..];
    let content = content.strip_suffix(b"\r\n").unwrap_or(content);
    let param = |key: &str| {
        head.split(&format!("{key}=\""))
            .nth(1)
            .and_then(|r| r.split('"').next())
            .map(str::to_owned)
    };
    Some(FormPart {
        name: param(" name").or_else(|| param(";name"))?,
        file_name: param("filename"),
        bytes: content.to_vec(),
    })
}

/// What a [`proxy`] was asked to reach: `CONNECT host:port` tunnels and
/// absolute-form `GET http://...` forwards, in order.
pub type ProxyLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// A loopback HTTP proxy (tunnels and plain forwards) that records every
/// destination; its address and log.
///
/// # Errors
///
/// No loopback port can be bound.
pub async fn proxy() -> std::io::Result<(SocketAddr, ProxyLog)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let log = ProxyLog::default();
    let seen = std::sync::Arc::clone(&log);
    pmt_core::spawn::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            pmt_core::spawn::spawn(relay(client, std::sync::Arc::clone(&seen)));
        }
    });
    Ok((addr, log))
}

async fn relay(mut client: tokio::net::TcpStream, log: ProxyLog) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        if client.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut words = text.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else {
        return;
    };
    log.lock()
        .expect("proxy log poisoned")
        .push(format!("{method} {target}"));
    let (authority, rewritten) = if method == "CONNECT" {
        (target.to_owned(), None)
    } else {
        let rest = target.strip_prefix("http://").unwrap_or(target);
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let request = text.replacen(target, &format!("/{path}"), 1);
        (authority.to_owned(), Some(request))
    };
    let Ok(mut upstream) = tokio::net::TcpStream::connect(&authority).await else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
        return;
    };
    let ready = match rewritten {
        None => {
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
        }
        Some(request) => upstream.write_all(request.as_bytes()).await,
    };
    if ready.is_ok() {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::multipart::Form;

    #[test]
    fn a_form_reads_back_into_its_parts() {
        let mut form = Form::new();
        form.text("chat_id", "42");
        form.file("photo", "a.png", b"P\r\nNG");
        let kind = form.content_type();
        let parts = form_parts(&kind, &form.finish());
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].name.as_str(), parts[0].text()),
            ("chat_id", "42".to_owned())
        );
        assert_eq!(parts[1].file_name.as_deref(), Some("a.png"));
        assert_eq!(parts[1].bytes, b"P\r\nNG");
    }
}
