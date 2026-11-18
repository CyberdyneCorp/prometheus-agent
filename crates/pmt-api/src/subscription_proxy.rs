//! The subscription proxy (spec: api-server, Subscription proxy): a local
//! OpenAI-compatible passthrough for a provider whose sign-in the profile
//! holds. No agent loop and no tools: an allowed path is forwarded
//! verbatim (streaming preserved) with the client's `Authorization`
//! replaced by the profile's current credential; any other path answers
//! 404 listing the allowed ones and nothing goes upstream. Requests for
//! another `Host`, from a browser (an `Origin`, or `Sec-Fetch-Site` other
//! than `none`) and, on a LAN listener, without the client key are
//! refused. Bodies are never logged; each forwarded call is handed to the
//! host for the usage ledger.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use pmt_core::cancel::CancelToken;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::{Body, Reply};

/// The default port.
pub const DEFAULT_PORT: u16 = 8645;
/// The largest request body forwarded.
const MAX_BODY: usize = 32 * 1024 * 1024;
/// The response tail kept to read its usage from.
const TAIL: usize = 64 * 1024;

/// A provider the proxy forwards to.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// The provider id (the ledger's `subscription:<provider>`).
    pub provider: String,
    /// The provider's API base (its `/v1`), without a trailing slash.
    pub base_url: String,
    /// The client paths forwarded (`/v1/chat/completions`, ...): each maps
    /// to the same path below `base_url`'s `/v1`.
    pub allowed: Vec<String>,
}

/// One forwarded call, for the usage ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxiedCall {
    /// The client path.
    pub path: String,
    /// The upstream status.
    pub status: u16,
    /// The model the request named.
    pub model: Option<String>,
    /// Input tokens the response reported.
    pub input_tokens: u64,
    /// Output tokens the response reported.
    pub output_tokens: u64,
}

/// What the proxy needs from the profile.
pub trait ProxyHost: Send + Sync {
    /// The provider's current credential, refreshed when due (under the
    /// cross-process refresh lock).
    fn credential(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;
    /// Records a forwarded call.
    fn record(&self, call: ProxiedCall);
}

/// A running proxy's configuration.
pub struct Proxy {
    /// Where requests go.
    pub upstream: Upstream,
    /// The bound address.
    pub listen: SocketAddr,
    /// The bearer LAN clients must present (`None` on loopback).
    pub client_key: Option<String>,
    /// The profile side.
    pub host: Arc<dyn ProxyHost>,
    /// The outbound client.
    pub client: reqwest::Client,
}

/// Serves `listener` until `stop`.
pub async fn serve(listener: tokio::net::TcpListener, proxy: Arc<Proxy>, stop: CancelToken) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        // The address this connection reached: on an all-interfaces bind it
        // is the one interface address the client may name in `Host`.
        let Ok(local) = stream.local_addr() else {
            continue;
        };
        let proxy = Arc::clone(&proxy);
        pmt_core::spawn::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let proxy = Arc::clone(&proxy);
                async move { Ok::<_, Infallible>(answer(&proxy, local, req).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(crate::HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service);
            if let Err(err) = conn.await {
                tracing::debug!("proxy connection ended: {err}");
            }
        });
    }
}

async fn answer(proxy: &Proxy, local: SocketAddr, req: Request<Incoming>) -> Reply {
    if let Some(refused) = refusal(proxy, local, req.headers()) {
        return refused;
    }
    let path = req.uri().path().to_owned();
    if !proxy.upstream.allowed.contains(&path) {
        return json(
            StatusCode::NOT_FOUND,
            &json!({"error": {"message": format!("{path} is not forwarded"),
                "type": "not_found", "allowed_paths": proxy.upstream.allowed}}),
        );
    }
    match forward(proxy, req, path).await {
        Ok(reply) => reply,
        Err((status, message)) => json(
            status,
            &json!({"error": {"message": message, "type": "proxy_error"}}),
        ),
    }
}

/// Why a request is refused before anything else: a foreign `Host`, a
/// browser, or a LAN client without the key.
fn refusal(proxy: &Proxy, local: SocketAddr, headers: &hyper::HeaderMap) -> Option<Reply> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !own_host(proxy.listen, local, header("host")) {
        return Some(json(
            StatusCode::FORBIDDEN,
            &json!({"error": {"message": "Host is not this proxy", "type": "forbidden"}}),
        ));
    }
    let site = header("sec-fetch-site");
    if header("origin").is_some() || site.is_some_and(|s| s != "none") {
        return Some(json(
            StatusCode::FORBIDDEN,
            &json!({"error": {"message": "browser requests are refused", "type": "forbidden"}}),
        ));
    }
    let key = proxy.client_key.as_deref()?;
    let presented = header("authorization").and_then(|v| v.strip_prefix("Bearer "));
    let ok =
        presented.is_some_and(|p| pmt_a2a::sign::constant_time_eq(p.as_bytes(), key.as_bytes()));
    (!ok).then(|| json(StatusCode::UNAUTHORIZED, &json!({"error": {"message": "this proxy needs its client key (PMT_PROXY_CLIENT_KEY)", "type": "unauthorized"}})))
}

/// Whether `host` names this listener: its port and its own address, as
/// an IP literal (a loopback address also as `localhost`). On an
/// all-interfaces bind the own address is the interface `local` the
/// connection arrived on, so a DNS name (a rebinding page) is refused there
/// too.
fn own_host(listen: SocketAddr, local: SocketAddr, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((name, port)) = host.rsplit_once(':') else {
        return false;
    };
    if port.parse::<u16>().ok() != Some(listen.port()) {
        return false;
    }
    let name = name.trim_start_matches('[').trim_end_matches(']');
    let own = if listen.ip().is_unspecified() {
        local.ip().to_canonical()
    } else {
        listen.ip()
    };
    name.parse::<std::net::IpAddr>().map_or_else(
        |_| own.is_loopback() && name.eq_ignore_ascii_case("localhost"),
        |named| named.to_canonical() == own,
    )
}

async fn forward(
    proxy: &Proxy,
    req: Request<Incoming>,
    path: String,
) -> Result<Reply, (StatusCode, String)> {
    let (parts, body) = req.into_parts();
    let body = Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "the request body is too large".to_owned(),
            )
        })?
        .to_bytes();
    let token = proxy
        .host
        .credential()
        .await
        .map_err(|why| (StatusCode::BAD_GATEWAY, why))?;
    let below = path.strip_prefix("/v1").unwrap_or(&path);
    let mut url = format!("{}{below}", proxy.upstream.base_url);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }
    let mut request = proxy.client.request(parts.method, url).bearer_auth(token);
    for (name, value) in &parts.headers {
        if forwarded_header(name.as_str()) {
            request = request.header(name, value);
        }
    }
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_owned));
    let response = request.body(body).send().await.map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "the provider could not be reached".to_owned(),
        )
    })?;
    let call = ProxiedCall {
        path,
        status: response.status().as_u16(),
        model,
        ..ProxiedCall::default()
    };
    Ok(stream_back(proxy, response, call))
}

/// Headers the client may pass on: never its credentials, cookies or
/// hop-by-hop headers.
fn forwarded_header(name: &str) -> bool {
    !matches!(
        name,
        "authorization"
            | "host"
            | "cookie"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-authorization"
            | "te"
            | "upgrade"
            | "origin"
    )
}

/// The upstream response streamed to the client as it arrives; its usage
/// is recorded once it ended.
fn stream_back(proxy: &Proxy, mut response: reqwest::Response, mut call: ProxiedCall) -> Reply {
    let mut builder = Response::builder().status(response.status().as_u16());
    for (name, value) in response.headers() {
        if forwarded_header(name.as_str()) {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
    }
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let host = Arc::clone(&proxy.host);
    pmt_core::spawn::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            tail.extend_from_slice(&chunk);
            if tail.len() > TAIL {
                tail.drain(..tail.len() - TAIL);
            }
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
        (call.input_tokens, call.output_tokens) = usage(&tail);
        host.record(call);
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(Frame::data(bytes)), rx))
    });
    builder
        .body(BodyExt::boxed(StreamBody::new(stream)))
        .unwrap_or_else(|_| {
            json(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": "bad upstream response"}}),
            )
        })
}

/// The last usage a response reported (a JSON body, or the event lines of
/// a stream): input and output tokens.
fn usage(tail: &[u8]) -> (u64, u64) {
    let text = String::from_utf8_lossy(tail);
    let whole = serde_json::from_str::<Value>(&text).ok();
    let events = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok());
    whole
        .into_iter()
        .chain(events)
        .filter_map(|v| v.get("usage").filter(|u| u.is_object()).cloned())
        .next_back()
        .map_or((0, 0), |u| {
            let count = |a: &str, b: &str| u[a].as_u64().or_else(|| u[b].as_u64()).unwrap_or(0);
            (
                count("prompt_tokens", "input_tokens"),
                count("completion_tokens", "output_tokens"),
            )
        })
}

fn json(status: StatusCode, body: &Value) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(BodyExt::boxed(Full::new(Bytes::from(body.to_string()))))
        .unwrap_or_else(|_| Response::new(Body::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_listener_s_own_host_is_served() {
        let loopback: SocketAddr = "127.0.0.1:8645".parse().unwrap();
        let own = |host| own_host(loopback, loopback, Some(host));
        assert!(own("127.0.0.1:8645"));
        assert!(own("localhost:8645"));
        assert!(!own("evil.example:8645"));
        assert!(!own("127.0.0.1:9000"));
        assert!(!own_host(loopback, loopback, None));
    }

    #[test]
    fn an_all_interfaces_listener_serves_only_the_address_a_client_reached() {
        let any: SocketAddr = "0.0.0.0:8645".parse().unwrap();
        let lan: SocketAddr = "192.168.1.20:8645".parse().unwrap();
        assert!(own_host(any, lan, Some("192.168.1.20:8645")));
        assert!(!own_host(any, lan, Some("evil.example:8645")));
        assert!(!own_host(any, lan, Some("localhost:8645")));
        assert!(!own_host(any, lan, Some("10.0.0.9:8645")));
        let loopback: SocketAddr = "127.0.0.1:8645".parse().unwrap();
        assert!(own_host(any, loopback, Some("localhost:8645")));
        let dual: SocketAddr = "[::]:8645".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.168.1.20]:8645".parse().unwrap();
        assert!(own_host(dual, mapped, Some("192.168.1.20:8645")));
    }

    #[test]
    fn usage_comes_from_a_body_or_the_last_stream_event() {
        assert_eq!(
            usage(br#"{"usage":{"prompt_tokens":12,"completion_tokens":3}}"#),
            (12, 3)
        );
        let stream = b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
        assert_eq!(usage(stream), (7, 2));
        assert_eq!(usage(b"not json"), (0, 0));
    }
}
