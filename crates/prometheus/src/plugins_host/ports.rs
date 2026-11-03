//! The ports the plugin host interfaces reach in the binary: secrets,
//! outbound HTTP, model calls (see [`super::llm`]) and the plugin's own
//! settings. Tool dispatch for plugins has no port yet and answers "not
//! available on this surface".

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::write::ConfigFile;
use pmt_net::OutboundPolicy;
use pmt_plugins::host::ports::{
    ConfigPort, HttpPort, HttpRequest, HttpResponse, PortFuture, Ports, SecretsPort,
};

/// The ports of `scope`'s plugin instances.
pub(super) fn ports(scope: &ProfileScope) -> Ports {
    let mut ports = Ports::default();
    let secrets = scope.secrets().clone();
    ports.secrets = Arc::new(Secrets(secrets));
    if let Some(http) = Http::new(scope) {
        ports.http = Arc::new(http);
    }
    ports.llm = Arc::new(super::llm::SessionsLlm);
    ports.config = Arc::new(Settings {
        file: scope.profile().config_file(),
        entries: scope.config().plugins.entries.clone(),
    });
    ports
}

struct Secrets(pmt_config::secrets::SecretsHandle);

impl SecretsPort for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        self.0.resolve(name).ok().map(|s| s.expose().to_owned())
    }
}

/// Outbound HTTP for plugins. Redirects are not followed: each hop must
/// come back through the host's `network` check. A request is bounded by
/// `plugins.wasm.call_timeout_ms`: the plugin call waiting on it holds its
/// instance, and the epoch deadline does not run while the host blocks.
struct Http {
    client: reqwest::Client,
}

impl Http {
    fn new(scope: &ProfileScope) -> Option<Self> {
        let config = scope.config();
        let network = &config.network;
        let policy = OutboundPolicy {
            force_ipv4: network.force_ipv4,
            extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
            proxy: network.proxy.clone(),
        };
        let client = pmt_net::http::client_builder(&policy)
            .ok()?
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(
                config.plugins.wasm.call_timeout_ms,
            ))
            .build()
            .ok()?;
        Some(Self { client })
    }
}

impl HttpPort for Http {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| format!("invalid method: {e}"))?;
            let mut builder = self.client.request(method, &request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(|e| failure(&e))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = response.bytes().await.map_err(|e| failure(&e))?.to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

/// What a plugin sees of a failed request: a timeout by name, else the
/// error with its causes (reqwest's own text hides them).
fn failure(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "request timed out (plugins.wasm.call_timeout_ms)".to_owned();
    }
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(text, ": {cause}");
        source = cause.source();
    }
    text
}

/// `plugins.entries.<id>.*`: read from the snapshot the session started
/// with, written to `config.toml` (taking effect for later sessions).
struct Settings {
    file: ConfigFile,
    entries:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

impl ConfigPort for Settings {
    fn get(&self, plugin: &str, key: &str) -> Option<serde_json::Value> {
        self.entries.get(plugin)?.get(key).cloned()
    }

    fn set(&self, plugin: &str, key: &str, value: serde_json::Value) -> Result<(), String> {
        self.file
            .set(
                &format!("plugins.entries.{plugin}.{key}"),
                &value.to_string(),
            )
            .map_err(|e| e.message().to_owned())
    }
}
