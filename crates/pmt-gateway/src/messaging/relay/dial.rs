//! Dialing the connector: TLS (`wss://`, verified by the platform trust
//! store plus `network.extra_ca_file`) or plain `ws://` to a loopback host
//! only (a local test connector), with the gateway credential as a bearer
//! token. The credential is never logged.

use std::path::Path;

use pmt_core::secret::SecretString;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// The connected socket.
pub(crate) type Socket = crate::messaging::ws::Socket;

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
    super::super::ws::connect(request, extra_ca_file)
        .await
        .map_err(|e| format!("the relay connection failed: {e}"))
}
