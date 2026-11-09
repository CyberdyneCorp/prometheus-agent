//! Detecting a llama-server someone else runs (spec: local-models,
//! External llama-server detection): `127.0.0.1:8080` and each port of
//! `local_runtime.detect_ports` are asked for `/props` and `/v1/models`;
//! a port answering both with JSON is a llama-server.

use std::time::Duration;

use serde_json::Value;

/// The port llama-server listens on by default.
pub const DEFAULT_PORT: u16 = 8080;

/// The ports probed, in order: 8080, then `extra` (without repeats).
#[must_use]
pub fn ports(extra: &[u16]) -> Vec<u16> {
    let mut ports = vec![DEFAULT_PORT];
    for port in extra {
        if !ports.contains(port) {
            ports.push(*port);
        }
    }
    ports
}

/// The first of `ports` serving a llama-server.
pub async fn first(client: &reqwest::Client, ports: &[u16]) -> Option<u16> {
    for port in ports {
        if is_llama_server(client, *port).await {
            return Some(*port);
        }
    }
    None
}

/// Whether `127.0.0.1:port` answers `/props` and `/v1/models` as a
/// llama-server does.
pub async fn is_llama_server(client: &reqwest::Client, port: u16) -> bool {
    let root = format!("http://127.0.0.1:{port}");
    let props = json(client, &format!("{root}/props")).await;
    let models = json(client, &format!("{root}/v1/models")).await;
    props.is_some_and(|p| p.is_object())
        && models.is_some_and(|m| m.get("data").is_some_and(Value::is_array))
}

async fn json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_slice(&response.bytes().await.ok()?).ok()
}

/// A client for loopback probes: no proxy, short connect timeout.
///
/// # Errors
///
/// `net.client_init` when the HTTP stack cannot be initialised.
pub fn loopback_client() -> Result<reqwest::Client, pmt_core::Error> {
    pmt_net::client_builder(&pmt_net::OutboundPolicy::default())?
        .no_proxy()
        .connect_timeout(Duration::from_millis(500))
        .build()
        .map_err(|e| pmt_core::Error::new(&pmt_core::error::codes::NET_CLIENT_INIT, e.to_string()))
}

/// [`loopback_client`], built on first use (most processes never probe).
#[must_use]
pub fn lazy_loopback_client() -> pmt_net::LazyClient {
    pmt_net::LazyClient::new(loopback_client)
}
