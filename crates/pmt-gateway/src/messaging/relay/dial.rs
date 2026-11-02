//! Dialing the connector: TLS (`wss://`, verified by the platform trust
//! store plus `network.extra_ca_file`) or plain `ws://` to a loopback host
//! only (a local test connector), with the gateway credential as a bearer
//! token. The credential is never logged.

use std::path::Path;
use std::sync::Arc;

use pmt_core::secret::SecretString;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// A byte stream, TLS or plain.
pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// The connected socket.
pub(crate) type Socket = WebSocketStream<Box<dyn Stream>>;

/// Opens the WebSocket at `url`.
///
/// # Errors
///
/// What failed, without the credential.
pub(crate) async fn dial(
    url: &str,
    token: &SecretString,
    gateway_id: Option<&str>,
    extra_ca_file: Option<&Path>,
) -> Result<Socket, String> {
    if !pmt_config::daemon_relay::secure_url(url, "wss", "ws") {
        return Err(format!(
            "refusing to send the relay credential to {url}: use wss:// (ws:// only to a loopback host)"
        ));
    }
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("bad relay URL {url}: {e}"))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| "the relay credential is not a valid header value".to_owned())?;
    request.headers_mut().insert("Authorization", bearer);
    if let Some(id) = gateway_id
        && let Ok(value) = HeaderValue::from_str(id)
    {
        request
            .headers_mut()
            .insert("X-Prometheus-Gateway-Id", value);
    }
    let uri = request.uri().clone();
    let host = uri
        .host()
        .ok_or_else(|| format!("relay URL {url} has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let tls = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot reach the relay at {host}:{port}: {e}"))?;
    let stream: Box<dyn Stream> = if tls {
        let config = pmt_net::tls::client_config(extra_ca_file).map_err(|e| e.to_string())?;
        let name = ServerName::try_from(host.clone())
            .map_err(|_| format!("relay host {host} is not a valid TLS name"))?;
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS with the relay at {host} failed: {e}"))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|e| format!("the relay refused the connection: {e}"))?;
    Ok(socket)
}
