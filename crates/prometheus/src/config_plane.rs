//! The HTTP client of the remote configuration backend (spec:
//! configuration-and-profiles, Remote configuration backend), registered
//! with `pmt_config::remote` at startup. Configuration loads are
//! synchronous and may run inside the daemon's runtime, so each request
//! runs on a thread of its own with a single-threaded runtime.

use std::sync::Arc;
use std::time::Duration;

use pmt_config::remote::{Reply, Transport};
use pmt_core::secret::SecretString;

/// One request's time limit (the first fetch retries within 30 s).
const TIMEOUT: Duration = Duration::from_secs(10);

/// The transport over the shared outbound client (platform trust store).
struct Http;

/// Registers the transport for this process.
pub fn register() {
    pmt_config::remote::register_transport(Arc::new(Http));
}

impl Transport for Http {
    fn get(&self, url: &str, token: &SecretString) -> Result<Reply, String> {
        let (url, token) = (url.to_owned(), token.clone());
        send(move |client| {
            client
                .get(url)
                .bearer_auth(token.expose())
                .header("accept", "application/json")
        })
    }

    fn patch(
        &self,
        url: &str,
        token: &SecretString,
        version: &str,
        body: &[u8],
    ) -> Result<Reply, String> {
        let (url, token, version, body) = (
            url.to_owned(),
            token.clone(),
            version.to_owned(),
            body.to_vec(),
        );
        send(move |client| {
            client
                .patch(url)
                .bearer_auth(token.expose())
                .header("if-match", version)
                .header("content-type", "application/json")
                .body(body)
        })
    }
}

fn send(
    request: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
) -> Result<Reply, String> {
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async move {
            let client = pmt_net::http::client_builder(&pmt_net::http::OutboundPolicy::default())
                .map_err(|e| e.message().to_owned())?
                .timeout(TIMEOUT)
                .build()
                .map_err(|e| e.to_string())?;
            let response = request(&client).send().await.map_err(|e| e.to_string())?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            Ok(Reply {
                status,
                body: body.to_vec(),
            })
        })
    });
    worker
        .join()
        .unwrap_or_else(|_| Err("the request thread panicked".to_owned()))
}
