//! The binary's side of the plugin host (spec: plugin-system): one wasmtime
//! runtime per process, started only when some plugin is to be loaded, one
//! set of plugin instances per profile (reused by every session of the
//! profile until its plugins, their files or their grants change), and the
//! ports the host interfaces reach: secrets, outbound HTTP and the plugin's
//! own settings. The event bus, model calls and tool dispatch for plugins
//! have no port yet and answer "not available".

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pmt_agent::turn::TurnHooks;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_config::write::ConfigFile;
use pmt_net::OutboundPolicy;
use pmt_plugins::hooks::{HookSettings, Hooks, PluginToolHooks};
use pmt_plugins::host::ports::{
    ConfigPort, HttpPort, HttpRequest, HttpResponse, PortFuture, Ports, SecretsPort,
};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::{LoadInputs, Loaded, State, Status, load, survey};
use pmt_tools::dispatch::{NoHooks, ToolHooks};
use semver::Version;

/// The running Prometheus version, as plugin manifests compare it.
#[must_use]
pub fn running_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// What one profile's sessions share.
pub struct ProfilePlugins {
    /// The loaded plugins and their hooks.
    pub loaded: Arc<Loaded>,
}

impl ProfilePlugins {
    /// The dispatcher's hook seam for one session.
    #[must_use]
    pub fn tool_hooks(&self, call: CallInfo) -> Arc<dyn ToolHooks> {
        if self.loaded.plugins.is_empty() {
            return Arc::new(NoHooks);
        }
        Arc::new(PluginToolHooks::new(Arc::clone(&self.loaded.hooks), call))
    }

    /// The turn loop's hook seam for one session (`None` when no plugin
    /// handles a turn hook).
    #[must_use]
    pub fn turn_hooks(&self, call: CallInfo) -> Option<Arc<dyn TurnHooks>> {
        self.loaded.hooks.handles("pre_llm_call").then(|| {
            Arc::new(SessionTurnHooks {
                hooks: Arc::clone(&self.loaded.hooks),
                call,
            }) as Arc<dyn TurnHooks>
        })
    }
}

struct SessionTurnHooks {
    hooks: Arc<Hooks>,
    call: CallInfo,
}

impl TurnHooks for SessionTurnHooks {
    fn pre_llm_call<'a>(
        &'a self,
        session_id: &'a str,
        user_message: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(
            self.hooks
                .pre_llm_call(session_id, user_message, &self.call),
        )
    }
}

/// Each profile's current load, with the fingerprint it was made for.
type Cache = HashMap<PathBuf, (String, Arc<Loaded>)>;

/// The profile's plugins for a session in `cwd`: the cached set when
/// nothing changed, else a fresh load. Plugin failures never fail the
/// session; they are logged and shown by `prometheus plugins list`.
#[must_use]
pub fn for_session(scope: &ProfileScope, cwd: &Path) -> ProfilePlugins {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let version = running_version();
    let inputs = inputs(scope, cwd, &version);
    let surveyed = survey(&inputs);
    let fingerprint = fingerprint(scope, &surveyed.iter().map(|(_, s)| s).collect::<Vec<_>>());
    let cache = CACHE.get_or_init(Mutex::default);
    let key = scope.dir().to_path_buf();
    if let Some((seen, loaded)) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        && *seen == fingerprint
    {
        return ProfilePlugins {
            loaded: Arc::clone(loaded),
        };
    }
    let loaded = Arc::new(load_now(
        scope,
        &inputs,
        surveyed.into_iter().map(|(_, s)| s).collect(),
    ));
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, (fingerprint, Arc::clone(&loaded)));
    ProfilePlugins { loaded }
}

/// Loads when some plugin is to be instantiated; otherwise reports the
/// survey without starting the WASM engine.
fn load_now(scope: &ProfileScope, inputs: &LoadInputs<'_>, statuses: Vec<Status>) -> Loaded {
    let settings = hook_settings(scope);
    if !statuses.iter().any(|s| s.state == State::Loaded) {
        return Loaded {
            plugins: Vec::new(),
            statuses,
            hooks: Arc::new(Hooks::new(settings)),
        };
    }
    match runtime() {
        Some(runtime) => load(runtime, &ports(scope), inputs),
        None => Loaded {
            plugins: Vec::new(),
            statuses,
            hooks: Arc::new(Hooks::new(settings)),
        },
    }
}

fn hook_settings(scope: &ProfileScope) -> HookSettings {
    let config = &scope.config().plugins;
    HookSettings {
        profile_dir: scope.dir().to_path_buf(),
        timeout: std::time::Duration::from_millis(config.hook_timeout_ms),
        context_max_chars: usize::try_from(config.hook_context_max_chars).unwrap_or(usize::MAX),
    }
}

/// The process's runtime (`None`, logged once, when the engine cannot start).
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            Runtime::new()
                .inspect_err(|err| tracing::error!("plugins unavailable: {}", err.message()))
                .ok()
        })
        .as_ref()
}

/// What decides whether a cached load is still current: the plugins'
/// identities and states, their settings and the grants file.
fn fingerprint(scope: &ProfileScope, statuses: &[&Status]) -> String {
    let grants = std::fs::metadata(scope.dir().join("grants.toml"))
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    // The whole section: limits, hook bounds and the settings the config
    // port serves.
    let mut text = format!("{grants:?}|{:?}|", scope.config().plugins);
    for status in statuses {
        let _ = write!(
            text,
            "{}={:?}:{:?}:{:?};",
            status.id, status.digest, status.state, status.problem
        );
    }
    text
}

/// The load inputs of `scope` for a session in `cwd`.
#[must_use]
pub fn inputs<'a>(scope: &'a ProfileScope, cwd: &'a Path, version: &'a Version) -> LoadInputs<'a> {
    LoadInputs {
        profile: scope.name().as_str(),
        profile_dir: scope.dir(),
        cwd: Some(cwd),
        config: &scope.config().plugins,
        embedded: &[],
        running: version,
    }
}

fn ports(scope: &ProfileScope) -> Ports {
    let mut ports = Ports::default();
    let secrets = scope.secrets().clone();
    ports.secrets = Arc::new(Secrets(secrets));
    if let Some(http) = Http::new(scope) {
        ports.http = Arc::new(http);
    }
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
/// come back through the host's `network` check.
struct Http {
    client: reqwest::Client,
}

impl Http {
    fn new(scope: &ProfileScope) -> Option<Self> {
        let network = &scope.config().network;
        let policy = OutboundPolicy {
            force_ipv4: network.force_ipv4,
            extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
            proxy: network.proxy.clone(),
        };
        let client = pmt_net::http::client_builder(&policy)
            .ok()?
            .redirect(reqwest::redirect::Policy::none())
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
            let response = builder.send().await.map_err(|e| e.to_string())?;
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
            let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
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
