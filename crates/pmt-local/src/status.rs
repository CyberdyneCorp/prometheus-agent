//! `prometheus local status`: the engine, the server, its resident models
//! and the machine's GPU and RAM use.

use std::path::Path;

use pmt_config::daemon::DaemonConfig;
use pmt_core::error::Error;
use serde::Serialize;
use serde_json::Value;

use crate::backend;
use crate::engine::Engine;
use crate::hardware::Hardware;
use crate::server::{detect, record};

/// The engine part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineStatus {
    /// The llama.cpp release this binary pins.
    pub pinned: String,
    /// The backend `local_runtime.backend` resolves to on this machine.
    pub backend: String,
    /// Installed builds as `<version>-<backend>`.
    pub installed: Vec<String>,
}

/// The server part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerStatus {
    /// `running` or `stopped`.
    pub state: &'static str,
    /// `managed` (Prometheus supervises it) or `external`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    /// Its loopback port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The managed server's pid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Whether sessions may start a managed server.
    pub enabled: bool,
}

/// The whole status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// The engine.
    pub engine: EngineStatus,
    /// The server.
    pub server: ServerStatus,
    /// Models loaded in the server now.
    pub resident: Vec<String>,
    /// GPUs and RAM, with what is in use.
    pub hardware: Hardware,
}

/// The status of `home` on `hardware`.
///
/// # Errors
///
/// `config.parse` for a bad `daemon.toml`; `net.client_init`.
pub async fn status(home: &Path, hardware: Hardware) -> Result<Status, Error> {
    let config = DaemonConfig::load(home)?.local_runtime;
    let engine = Engine::open(home);
    let selected = backend::select(config.backend, &hardware);
    let client = detect::loopback_client()?;
    let managed = crate::server::recorded(home);
    let external = if managed.is_none() {
        detect::first(&client, &detect::ports(&config.detect_ports)).await
    } else {
        None
    };
    let port = managed.as_ref().map(|r| r.port).or(external);
    let resident = match port {
        Some(port) => resident(&client, port, managed.is_some()).await,
        None => Vec::new(),
    };
    Ok(Status {
        engine: EngineStatus {
            pinned: engine.index().version.clone(),
            backend: selected.backend.to_string(),
            installed: engine
                .installed_all()
                .iter()
                .map(|b| format!("{}-{}", b.record.version, b.record.backend))
                .collect(),
        },
        server: ServerStatus {
            state: if port.is_some() { "running" } else { "stopped" },
            source: match (&managed, external) {
                (Some(_), _) => Some("managed"),
                (None, Some(_)) => Some("external"),
                _ => None,
            },
            port,
            pid: managed.as_ref().map(|r| r.pid),
            enabled: config.enabled,
        },
        resident,
        hardware,
    })
}

/// Loaded models: the router's `/models` entries whose status is
/// `loaded`; a single-model server's `/v1/models`.
async fn resident(client: &reqwest::Client, port: u16, router: bool) -> Vec<String> {
    let path = if router { "models" } else { "v1/models" };
    let Ok(response) = client
        .get(format!("http://127.0.0.1:{port}/{path}"))
        .send()
        .await
    else {
        return Vec::new();
    };
    let Ok(body) = response.bytes().await else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return Vec::new();
    };
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| !router || m.pointer("/status/value").and_then(Value::as_str) == Some("loaded"))
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// The record file path, for messages.
#[must_use]
pub fn record_path(home: &Path) -> std::path::PathBuf {
    record::path(home)
}
