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
        let config = scope.config();
        let outbound = pmt_net::OutboundPolicy {
            force_ipv4: config.network.force_ipv4,
            extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
            proxy: config.network.proxy.clone(),
        };
        let egress = &config.security.egress;
        let hosts = crate::egress::host_policy(egress, &crate::egress::Overrides::default());
        let fetcher =
            pmt_net::fetch::Fetcher::new(&outbound, crate::egress::destinations(egress)?)?
                .with_hosts(
                    std::sync::Arc::new(hosts),
                    requester,
                    Some(crate::egress::observer(scope, None, None)),
                );
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
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BODY {
            return Err(format!(
                "the answer is larger than {} MiB",
                MAX_BODY / (1024 * 1024)
            ));
        }
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
