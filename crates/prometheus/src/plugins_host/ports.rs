//! The ports the plugin host interfaces reach in the binary: secrets,
//! outbound HTTP, model calls (see [`super::llm`]) and the plugin's own
//! settings. Tool dispatch for plugins has no port yet and answers "not
//! available on this surface".

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::write::ConfigFile;
use pmt_net::destination::PrivateGrant;
use pmt_net::{Fetcher, OutboundPolicy};
use pmt_plugins::host::ports::{
    ConfigPort, HttpPort, HttpRequest, HttpResponse, PortFuture, Ports, SecretsPort,
};

/// The ports of `scope`'s plugin instances.
pub(super) fn ports(scope: &ProfileScope) -> Ports {
    let mut ports = Ports::default();
    let secrets = scope.secrets().clone();
    ports.secrets = Arc::new(Secrets(secrets));
    ports.http = Arc::new(Http::new(scope));
    ports.llm = Arc::new(super::llm::SessionsLlm);
    ports.config = Arc::new(Settings {
        file: scope.profile().config_file(),
        entries: scope.config().plugins.entries.clone(),
    });
    #[cfg(feature = "gateway")]
    {
        ports.messaging = Arc::new(super::messaging::GatewayMessaging {
            profile: scope.name().to_string(),
        });
    }
    ports
}

struct Secrets(pmt_config::secrets::SecretsHandle);

impl SecretsPort for Secrets {
    fn get(&self, name: &str) -> Option<String> {
        self.0.resolve(name).ok().map(|s| s.expose().to_owned())
    }
}

/// Outbound HTTP for plugins. The host already checked the plugin's
/// `network` grant; this port adds the egress policy every outbound
/// connection obeys (spec: security-boundaries, Per-session network egress
/// policy; Private and metadata destinations): the calling session's mode
/// and patterns (the profile's outside a session), metadata endpoints never,
/// private addresses only with `allow_private` or a grant naming the host.
/// A refusal is an `egress.denied` event of the calling session and an
/// audit record. Redirects are not followed: each hop must come back
/// through the host's `network` check. A request is bounded by
/// `plugins.wasm.call_timeout_ms`: the plugin call waiting on it holds its
/// instance, and the epoch deadline does not run while the host blocks.
/// The request ends a margin before that bound (see [`request_bound`]).
struct Http {
    scope: ProfileScope,
    outbound: OutboundPolicy,
    timeout: Duration,
    /// One fetcher per (plugin, session, granted hosts): the grants and
    /// the session decide its private grants and where refusals go.
    fetchers: Mutex<HashMap<FetcherKey, Arc<Fetcher>>>,
}

type FetcherKey = (String, Option<String>, Vec<String>);

/// Fetchers kept before the cache starts over (sessions come and go).
const FETCHERS_KEPT: usize = 64;

impl Http {
    fn new(scope: &ProfileScope) -> Self {
        let config = scope.config();
        let network = &config.network;
        Self {
            scope: scope.clone(),
            outbound: OutboundPolicy {
                force_ipv4: network.force_ipv4,
                extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
                proxy: network.proxy.clone(),
            },
            timeout: request_bound(Duration::from_millis(config.plugins.wasm.call_timeout_ms)),
            fetchers: Mutex::default(),
        }
    }

    fn fetcher(&self, request: &HttpRequest) -> Result<Arc<Fetcher>, String> {
        let key = (
            request.plugin.clone(),
            request.session_id.clone(),
            request.granted_hosts.clone(),
        );
        let mut fetchers = self.fetchers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fetcher) = fetchers.get(&key) {
            return Ok(Arc::clone(fetcher));
        }
        let config = self.scope.config();
        let mut destinations =
            crate::egress::destinations(&config.security.egress).map_err(|e| e.to_string())?;
        for pattern in &request.granted_hosts {
            if let Ok(grant) = PrivateGrant::parse(pattern) {
                destinations = destinations.with_grant(grant);
            }
        }
        let session = request.session_id.clone();
        let hosts = crate::egress::hosts_for(&self.scope, session.as_deref());
        let caller: crate::egress::Caller = Arc::new(move || session.clone());
        let observer = crate::egress::attributed(&self.scope, caller);
        let fetcher = Fetcher::new(&self.outbound, destinations)
            .map_err(|e| e.to_string())?
            .with_hosts(hosts, &format!("plugin:{}", request.plugin), Some(observer));
        let fetcher = Arc::new(fetcher);
        if fetchers.len() >= FETCHERS_KEPT {
            fetchers.clear();
        }
        fetchers.insert(key, Arc::clone(&fetcher));
        Ok(fetcher)
    }

    async fn exchange(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| format!("invalid method: {e}"))?;
        let fetcher = self.fetcher(&request)?;
        let HttpRequest {
            url, headers, body, ..
        } = request;
        let response = fetcher
            .send(method, &url, |mut builder| {
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                if let Some(body) = body {
                    builder = builder.body(body);
                }
                builder
            })
            .await
            .map_err(|e| e.message().to_owned())?;
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
    }
}

impl HttpPort for Http {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            tokio::time::timeout(self.timeout, self.exchange(request))
                .await
                .unwrap_or_else(|_| {
                    Err("request timed out (plugins.wasm.call_timeout_ms)".to_owned())
                })
        })
    }
}

/// How long one plugin HTTP request may take under a call bound of
/// `call_timeout`: a quarter less (at most a second less), so a timed-out
/// request returns to the plugin while its call still has time to report
/// it. Ending at the bound itself races the call's own wall deadline: the
/// call would then trap with "exceeded call timeout" as soon as control
/// came back, and the plugin would never see why.
fn request_bound(call_timeout: Duration) -> Duration {
    call_timeout.saturating_sub((call_timeout / 4).min(Duration::from_secs(1)))
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

#[cfg(test)]
mod tests {
    //! Per-session network egress policy and private/metadata destinations
    //! for WASM plugin HTTP (spec: security-boundaries).

    use pmt_config::{Home, Launch, Overrides, ProfileName};

    use super::*;

    #[test]
    fn a_request_ends_well_before_its_call_does() {
        // Several epoch ticks of headroom: the call's deadline is checked
        // once per tick.
        for ms in [500, 30_000] {
            let call = Duration::from_millis(ms);
            let request = request_bound(call);
            assert!(request > Duration::ZERO, "{ms}");
            assert!(
                call.saturating_sub(request) >= 5 * pmt_plugins::host::TICK,
                "{ms}"
            );
        }
    }

    fn scope(root: &std::path::Path, config: &str) -> ProfileScope {
        let home = Home::open(root).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        std::fs::write(profile.path().join("config.toml"), config).unwrap();
        ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
    }

    fn get(url: &str, granted_hosts: &[&str]) -> HttpRequest {
        HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            plugin: "weather".to_owned(),
            session_id: Some("ses_plugin".to_owned()),
            granted_hosts: granted_hosts.iter().map(|h| (*h).to_owned()).collect(),
        }
    }

    fn audit(scope: &ProfileScope) -> String {
        std::fs::read_to_string(scope.dir().join(pmt_core::audit::FILE)).unwrap_or_default()
    }

    /// A local server answering `ok` to one request.
    fn local_server() -> u16 {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            }
        });
        port
    }

    #[tokio::test]
    async fn egress_mode_none_refuses_a_plugin_request() {
        let root = tempfile::tempdir().unwrap();
        let scope = scope(root.path(), "[security.egress]\nmode = \"none\"\n");
        let http = Http::new(&scope);
        let err = http
            .send(get(
                "https://api.weather.example/today",
                &["api.weather.example"],
            ))
            .await
            .unwrap_err();
        assert!(err.contains("egress policy (mode_none)"), "{err}");
        let audit = audit(&scope);
        assert!(audit.contains("api.weather.example:443"), "{audit}");
        assert!(audit.contains("plugin:weather (mode_none)"), "{audit}");
        assert!(audit.contains("ses_plugin"), "{audit}");
    }

    #[tokio::test]
    async fn metadata_and_private_addresses_are_refused_under_a_wildcard_grant() {
        let root = tempfile::tempdir().unwrap();
        let scope = scope(root.path(), "");
        let http = Http::new(&scope);
        let metadata = http
            .send(get("http://169.254.169.254/latest/meta-data", &[]))
            .await
            .unwrap_err();
        assert!(metadata.contains("metadata"), "{metadata}");
        let port = local_server();
        let private = http
            .send(get(&format!("http://127.0.0.1:{port}/"), &[]))
            .await
            .unwrap_err();
        assert!(private.contains("private"), "{private}");
        let audit = audit(&scope);
        assert!(audit.contains("plugin:weather (metadata)"), "{audit}");
        assert!(audit.contains("plugin:weather (private_range)"), "{audit}");
        // A grant naming the private host reaches it.
        let named = http
            .send(get(&format!("http://127.0.0.1:{port}/"), &["127.0.0.1"]))
            .await
            .unwrap();
        assert_eq!(named.body, b"ok");
    }
}
