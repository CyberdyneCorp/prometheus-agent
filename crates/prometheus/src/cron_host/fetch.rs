//! Monitored URLs (spec: scheduled-jobs, Monitor mode): a bounded GET
//! under the profile's egress rules, 30 s at most, the body capped at
//! 1 MiB. The agent can set `monitor_url`, so the fetch is an untrusted
//! one: every hop (redirects included) passes the egress host policy
//! (`security.egress`: mode, allowlist, blocklist) and the destination
//! policy (private, loopback and metadata addresses refused unless
//! `security.egress.allow_private`), over the profile's proxy, extra CA and
//! IPv4 preference.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_cron::exec::{MAX_MONITOR_BODY, MONITOR_TIMEOUT};
use pmt_net::OutboundPolicy;
use pmt_net::fetch::Fetcher;

/// Who the egress audit names for these fetches.
const REQUESTER: &str = "cron_monitor";

/// The fetcher of the profile's monitored URLs.
fn fetcher(scope: &ProfileScope) -> Result<Fetcher, Error> {
    let config = scope.config();
    let network = &config.network;
    let egress = &config.security.egress;
    let outbound = OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    let hosts = crate::egress::host_policy(egress, &crate::egress::Overrides::default());
    Ok(
        Fetcher::new(&outbound, crate::egress::destinations(egress)?)?.with_hosts(
            Arc::new(hosts),
            REQUESTER,
            None,
        ),
    )
}

/// The body of `url`, or why it could not be read.
pub(super) async fn get(scope: &ProfileScope, url: &str) -> Result<String, String> {
    let fetcher = fetcher(scope).map_err(|e| e.message().to_owned())?;
    let read = async {
        let mut response = fetcher.get(url).await.map_err(|e| e.message().to_owned())?;
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
