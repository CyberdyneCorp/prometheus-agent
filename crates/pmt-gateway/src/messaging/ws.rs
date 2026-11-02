//! WebSocket connections of adapters and the relay: TLS (`wss://`,
//! verified by the platform trust store plus `network.extra_ca_file`) or
//! plain `ws://` (local servers and test mocks).

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;

/// A byte stream, TLS or plain.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A connected WebSocket.
pub type Socket = WebSocketStream<Box<dyn Stream>>;

/// Opens the WebSocket of `request` (its URI decides TLS).
///
/// # Errors
///
/// What failed: unreachable host, TLS, or the handshake.
pub async fn connect(request: Request, extra_ca_file: Option<&Path>) -> Result<Socket, String> {
    let uri = request.uri().clone();
    let host = uri
        .host()
        .ok_or_else(|| format!("{uri} has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;
    let stream: Box<dyn Stream> = if tls {
        let config = pmt_net::tls::client_config(extra_ca_file).map_err(|e| e.to_string())?;
        let name = ServerName::try_from(host.clone())
            .map_err(|_| format!("{host} is not a valid TLS name"))?;
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS with {host} failed: {e}"))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| format!("the WebSocket handshake with {host} failed: {e}"))?;
    Ok(socket)
}
