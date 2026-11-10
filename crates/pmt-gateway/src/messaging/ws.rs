//! WebSocket connections of adapters and the relay: TLS (`wss://`,
//! verified by the platform trust store plus `network.extra_ca_file`) or
//! plain `ws://` (local servers and test mocks).

use std::path::Path;

use pmt_net::stream::{Route, Stream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;

/// A connected WebSocket.
pub type Socket = WebSocketStream<Box<dyn Stream>>;

/// Opens the WebSocket of `request` (its URI decides TLS).
///
/// # Errors
///
/// What failed: unreachable host, TLS, or the handshake.
pub async fn connect(request: Request, extra_ca_file: Option<&Path>) -> Result<Socket, String> {
    connect_via(request, extra_ca_file, None).await
}

/// [`connect`] through the proxy at `proxy` (see [`pmt_net::stream`]), or
/// directly.
///
/// # Errors
///
/// What failed: unreachable proxy or host, TLS, or the handshake.
pub async fn connect_via(
    request: Request,
    extra_ca_file: Option<&Path>,
    proxy: Option<&str>,
) -> Result<Socket, String> {
    let uri = request.uri().clone();
    let host = uri
        .host()
        .ok_or_else(|| format!("{uri} has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let route = Route {
        proxy,
        extra_ca_file,
        force_ipv4: false,
    };
    let stream = pmt_net::stream::open(&host, port, tls, route).await?;
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| format!("the WebSocket handshake with {host} failed: {e}"))?;
    Ok(socket)
}
