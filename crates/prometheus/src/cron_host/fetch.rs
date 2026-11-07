//! Monitored URLs (spec: scheduled-jobs, Monitor mode): a bounded GET
//! with the profile's outbound HTTP settings (its proxy, extra CA,
//! IPv4 preference), 30 s at most, the body capped at 1 MiB.

use pmt_config::ProfileScope;
use pmt_cron::exec::{MAX_MONITOR_BODY, MONITOR_TIMEOUT};
use pmt_net::OutboundPolicy;

/// The body of `url`, or why it could not be read.
pub(super) async fn get(scope: &ProfileScope, url: &str) -> Result<String, String> {
    let network = &scope.config().network;
    let client = pmt_net::client(&OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    })
    .map_err(|e| e.message().to_owned())?;
    let read = async {
        let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "{url} answered HTTP {}",
                response.status().as_u16()
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            let room = MAX_MONITOR_BODY.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() >= MAX_MONITOR_BODY {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    };
    tokio::time::timeout(MONITOR_TIMEOUT, read)
        .await
        .map_err(|_| {
            format!(
                "{url} did not answer within {} s",
                MONITOR_TIMEOUT.as_secs()
            )
        })?
}
