//! HTTP for the outbound A2A client and peer gateway calls: every request
//! goes through the profile's egress-checked fetcher, so an agent URL is
//! held to the same destination rules (private, loopback and metadata
//! addresses refused unless allowlisted) as any other outbound fetch.

use std::time::Duration;

use pmt_a2a::client::{BoxFuture, HttpReply, Transport};
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use serde_json::Value;

/// The largest answer read.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// HTTP through a profile's egress policy.
pub struct NetTransport {
    fetcher: pmt_net::fetch::Fetcher,
    timeout: Duration,
}

impl NetTransport {
    /// A transport under `scope`'s egress policy, each request bounded by
    /// `timeout`.
    ///
    /// # Errors
    ///
    /// A bad egress configuration; the client cannot be built.
    pub fn new(scope: &ProfileScope, requester: &str, timeout: Duration) -> Result<Self, Error> {
        let fetcher = crate::egress::fetcher(scope, requester, None)?;
        Ok(Self { fetcher, timeout })
    }

    /// One request; the status and JSON body (`null` when not JSON).
    ///
    /// # Errors
    ///
    /// The destination refusal or transport failure, as text.
    pub async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        token: Option<&str>,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<HttpReply, String> {
        let timeout = self.timeout;
        let response = self
            .fetcher
            .send(method, url, |mut request| {
                request = request
                    .timeout(timeout)
                    .header("accept", "application/json")
                    .header("A2A-Version", pmt_a2a::wire::PROTOCOL_VERSION);
                for (name, value) in headers {
                    request = request.header(*name, *value);
                }
                if let Some(token) = token {
                    request = request.bearer_auth(token);
                }
                match body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await
            .map_err(|e| e.message().to_owned())?;
        let status = response.status().as_u16();
        let bytes = pmt_net::fetch::read_capped(response, MAX_BODY)
            .await
            .map_err(|e| format!("the answer: {e}"))?;
        Ok(HttpReply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        })
    }
}

impl Transport for NetTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, Result<HttpReply, String>> {
        Box::pin(self.request(reqwest::Method::GET, url, token, None, &[]))
    }

    fn post<'a>(
        &'a self,
        url: &'a str,
        token: Option<&'a str>,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<HttpReply, String>> {
        Box::pin(self.request(reqwest::Method::POST, url, token, Some(body), &[]))
    }
}

/// A local server answering one request with a JSON status line and then
/// a body that never ends (until the client hangs up); its base URL. Shared
/// by the A2A transport and peer client tests.
#[cfg(test)]
pub(crate) async fn endless_server() -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    static CHUNK: [u8; 64 * 1024] = [b' '; 64 * 1024];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    pmt_core::spawn::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        while socket.write_all(&CHUNK).await.is_ok() {}
    });
    format!("http://{addr}")
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName};

    use super::*;

    /// A profile whose egress policy lets it reach the loopback stand-in.
    fn loopback_scope(root: &std::path::Path) -> ProfileScope {
        let home = Home::open(root).unwrap();
        std::fs::write(
            root.join("profiles/default/config.toml"),
            "[security.egress]\nallow_private = true\n",
        )
        .unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
    }

    /// A hostile agent streaming without end is cut off at the cap: the
    /// call fails instead of buffering the whole answer.
    #[tokio::test]
    async fn an_endless_answer_is_refused_at_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let scope = loopback_scope(root.path());
        let transport = NetTransport::new(&scope, "a2a", Duration::from_secs(30)).unwrap();
        let url = endless_server().await;
        let refused = transport.get(&url, None).await.unwrap_err();
        assert_eq!(refused, "the answer: larger than 8 MiB");
    }
}
