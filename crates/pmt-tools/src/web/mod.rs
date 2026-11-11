//! The `web` toolset: `web_search` and `web_extract` (spec: tool-system,
//! Single-purpose toolsets; web-and-media-tools; security-boundaries,
//! In-process fetch SSRF protection).
//!
//! Each tool calls the backend configured for its capability
//! (`web.search_backend` / `web.extract_backend` over `web.backend`). The
//! URLs the model asks to extract pass the session's destination checks
//! (private, metadata and unresolvable hosts refused) before any backend
//! sees them. Answers go through the result caches ([`cache`]); a failed
//! keyed backend may be rescued once by the opt-in keyless ring
//! ([`keyless`]).

pub mod backend;
pub mod cache;
pub mod full_text;
pub mod keyless;
pub mod select;
pub mod session;
pub mod tools;
pub mod vendors;
pub mod window;

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_config::schema::WebConfig;
use pmt_core::secret::SecretString;
use pmt_net::fetch::Fetcher;

pub use backend::{Firecrawl, Hit, Page, Searxng, SecretLookup, WebBackend};
pub use full_text::FullTextStore;
pub use select::{AUTO_ORDER, BACKENDS, HasSecret, NOTHING_CONFIGURED, not_installed, resolve};
pub use tools::tools;

use crate::usage::UsageSink;
use keyless::Ring;
use vendors::{Credential, Ddgs, Vendor, XaiSearch};

/// Backends enabled plugins provide, by capability and configured id
/// (`None`: no plugin provides it).
pub type PluginBackends<'a> = &'a dyn Fn(Capability, &str) -> Option<Arc<dyn WebBackend>>;

/// A token-bearing credential and its billing source, read at each call
/// (the xAI key or `xai-oauth` sign-in of the `xai` backend).
pub type TokenLookup = Arc<dyn Fn() -> Option<(SecretString, String)> + Send + Sync>;

/// Which web capability a backend serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `web_search`.
    Search,
    /// `web_extract`.
    Extract,
}

/// The subscription tool gateway's web route (the `nous` backend).
#[derive(Clone)]
pub struct GatewayRoute {
    /// The gateway URL.
    pub url: String,
    /// The bearer token.
    pub token: SecretString,
    /// `subscription:nous/<label>` (or the self-hosted gateway's).
    pub billing_source: String,
}

/// What a session's web tools are built from beside its config.
pub struct WebDeps<'a> {
    /// Calls the backends' APIs.
    pub client: &'a reqwest::Client,
    /// The session's destination policy for the URLs to extract.
    pub fetcher: Arc<Fetcher>,
    /// The profile's secrets.
    pub secrets: &'a SecretLookup,
    /// Where truncated pages and cached extractions are kept.
    pub full_text: FullTextStore,
    /// Backends of enabled plugins.
    pub plugins: PluginBackends<'a>,
    /// Base URL overrides by backend or keyless vendor id (mirrors, tests).
    pub endpoints: BTreeMap<String, String>,
    /// The tool gateway's web route, when the profile has one.
    pub gateway: Option<GatewayRoute>,
    /// The `xai` backend's credential (default: `XAI_API_KEY`).
    pub xai: Option<TokenLookup>,
    /// Where metered calls are recorded.
    pub usage: Option<UsageSink>,
}

impl<'a> WebDeps<'a> {
    /// Dependencies with no plugin, gateway, xAI sign-in, endpoint
    /// override or usage sink.
    #[must_use]
    pub fn new(
        client: &'a reqwest::Client,
        fetcher: Arc<Fetcher>,
        secrets: &'a SecretLookup,
        full_text: FullTextStore,
    ) -> Self {
        Self {
            client,
            fetcher,
            secrets,
            full_text,
            plugins: &|_, _| None,
            endpoints: BTreeMap::new(),
            gateway: None,
            xai: None,
            usage: None,
        }
    }
}

/// Who answers one capability.
#[derive(Clone)]
pub enum Source {
    /// One backend.
    Backend(Arc<dyn WebBackend>),
    /// The keyless ring.
    Keyless(Arc<Ring>),
    /// The model provider runs the search itself (`openai-native`).
    Provider,
}

/// One session's web tools.
pub struct WebSession {
    search: Result<Source, String>,
    extract: Result<Source, String>,
    /// The ring a failed keyed backend is rescued on (opt-in).
    rescue: Option<Arc<Ring>>,
    fetcher: Arc<Fetcher>,
    full_text: FullTextStore,
    config: WebConfig,
    usage: Option<UsageSink>,
}

impl WebSession {
    /// The session's backends from `config`. `client` calls the backends'
    /// APIs; `fetcher` carries the session's destination policy;
    /// `full_text` keeps the full text of truncated pages.
    #[must_use]
    pub fn from_config(
        config: &WebConfig,
        client: &reqwest::Client,
        fetcher: Arc<Fetcher>,
        secrets: &SecretLookup,
        full_text: FullTextStore,
    ) -> Self {
        Self::build(config, WebDeps::new(client, fetcher, secrets, full_text))
    }

    /// [`Self::from_config`] where a configured id this build does not
    /// implement may name a backend an enabled plugin provides (spec:
    /// plugin-system, Provider-object plugins fail closed when
    /// unregistered); an id nothing provides leaves the tool unavailable
    /// with `<category> provider '<id>' is not installed or not enabled`.
    #[must_use]
    pub fn from_config_with(
        config: &WebConfig,
        client: &reqwest::Client,
        fetcher: Arc<Fetcher>,
        secrets: &SecretLookup,
        full_text: FullTextStore,
        plugins: PluginBackends<'_>,
    ) -> Self {
        let mut deps = WebDeps::new(client, fetcher, secrets, full_text);
        deps.plugins = plugins;
        Self::build(config, deps)
    }

    /// The session's web tools from `config` and `deps`.
    #[must_use]
    pub fn build(config: &WebConfig, deps: WebDeps<'_>) -> Self {
        let (secrets, gateway, xai) = (deps.secrets, deps.gateway.is_some(), deps.xai.clone());
        let has_secret = move |name: &str| presence(secrets, gateway, xai.as_ref(), name);
        let mut ring: Option<Arc<Ring>> = None;
        let mut ring_of = || {
            Arc::clone(ring.get_or_insert_with(|| {
                Arc::new(Ring::new(
                    config,
                    deps.client,
                    deps.secrets,
                    &deps.endpoints,
                ))
            }))
        };
        let mut build = |capability| {
            let id = select::configured(config, capability);
            if id != "auto"
                && !BACKENDS.contains(&id)
                && let Some(plugin) = (deps.plugins)(capability, id)
            {
                return Ok(Source::Backend(plugin));
            }
            let id = resolve(config, capability, &has_secret)?;
            source(id, config, &deps, &mut ring_of)
        };
        let search = build(Capability::Search);
        let extract = build(Capability::Extract);
        let keyed = |s: &Result<Source, String>| matches!(s, Ok(Source::Backend(_)));
        let rescue = (config.keyless.enabled
            && config.keyless.rescue
            && (keyed(&search) || keyed(&extract)))
        .then(ring_of);
        Self {
            search,
            extract,
            rescue,
            fetcher: deps.fetcher,
            full_text: deps.full_text,
            config: config.clone(),
            usage: deps.usage,
        }
    }

    /// A session with explicit backends (tests, plugins).
    #[must_use]
    pub fn with_backends(
        config: &WebConfig,
        search: Option<Arc<dyn WebBackend>>,
        extract: Option<Arc<dyn WebBackend>>,
        fetcher: Arc<Fetcher>,
        full_text: FullTextStore,
    ) -> Self {
        let missing = || NOTHING_CONFIGURED.to_owned();
        Self {
            search: search.map(Source::Backend).ok_or_else(missing),
            extract: extract.map(Source::Backend).ok_or_else(missing),
            rescue: None,
            fetcher,
            full_text,
            config: config.clone(),
            usage: None,
        }
    }
}

/// Whether `name` is present: a secret, or a sign-in standing for the
/// gateway or xAI.
fn presence(secrets: &SecretLookup, gateway: bool, xai: Option<&TokenLookup>, name: &str) -> bool {
    if name == crate::probe::signin("nous") {
        return gateway;
    }
    if name == crate::probe::signin("xai-oauth") {
        return xai.is_some_and(|xai| xai().is_some());
    }
    secrets(name).is_some()
}

/// The source serving the resolved backend `id`.
fn source(
    id: &str,
    config: &WebConfig,
    deps: &WebDeps<'_>,
    ring: &mut dyn FnMut() -> Arc<Ring>,
) -> Result<Source, String> {
    let client = deps.client.clone();
    let endpoint = deps.endpoints.get(id).map(String::as_str);
    let backend: Arc<dyn WebBackend> = match id {
        "keyless" => return Ok(Source::Keyless(ring())),
        "openai-native" => return Ok(Source::Provider),
        "searxng" => Arc::new(Searxng::new(
            client,
            config.searxng.url.as_deref().unwrap_or_default(),
        )),
        "firecrawl" => Arc::new(Firecrawl::new(
            client,
            config.firecrawl.base_url.as_deref(),
            Arc::clone(deps.secrets),
        )),
        "ddgs" => Arc::new(Ddgs::new(client, endpoint)),
        "xai" => {
            let secrets = Arc::clone(deps.secrets);
            let credential: TokenLookup = deps.xai.clone().unwrap_or_else(|| {
                Arc::new(move || secrets("XAI_API_KEY").map(|key| (key, "api_key:xai".to_owned())))
            });
            Arc::new(XaiSearch::new(
                client,
                endpoint,
                config.xai.clone(),
                credential,
            ))
        }
        "nous" => {
            let gateway = deps
                .gateway
                .clone()
                .ok_or_else(|| "backend 'nous' needs a Nous Portal sign-in".to_owned())?;
            let base = endpoint.map_or_else(|| gateway.url.clone(), str::to_owned);
            Arc::new(Vendor::new(
                &vendors::NOUS,
                client,
                Some(&base),
                Credential::Token(gateway.token, gateway.billing_source),
            ))
        }
        other => {
            let spec = vendors::spec(other).ok_or_else(|| format!("unknown backend '{other}'"))?;
            Arc::new(Vendor::new(
                spec,
                client,
                endpoint,
                Credential::Key(Arc::clone(deps.secrets)),
            ))
        }
    };
    Ok(Source::Backend(backend))
}
