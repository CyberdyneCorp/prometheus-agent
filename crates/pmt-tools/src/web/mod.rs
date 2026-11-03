//! The `web` toolset: `web_search` and `web_extract` (spec: tool-system,
//! Single-purpose toolsets; web-and-media-tools; security-boundaries,
//! In-process fetch SSRF protection).
//!
//! Each tool calls the backend configured for its capability
//! (`web.search_backend` / `web.extract_backend` over `web.backend`). The
//! URLs the model asks to extract pass the session's destination checks
//! (private, metadata and unresolvable hosts refused) before any backend
//! sees them.

pub mod backend;
pub mod full_text;
pub mod select;
pub mod tools;
pub mod window;

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::WebConfig;
use pmt_net::fetch::Fetcher;

pub use backend::{Firecrawl, Hit, Page, Searxng, SecretLookup, WebBackend};
pub use full_text::FullTextStore;
pub use select::{AUTO_ORDER, BACKENDS, HasSecret, NOTHING_CONFIGURED, not_installed, resolve};

/// Backends enabled plugins provide, by capability and configured id
/// (`None`: no plugin provides it).
pub type PluginBackends<'a> = &'a dyn Fn(Capability, &str) -> Option<Arc<dyn WebBackend>>;
pub use tools::tools;

/// Which web capability a backend serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `web_search`.
    Search,
    /// `web_extract`.
    Extract,
}

/// One session's web tools.
pub struct WebSession {
    search: Result<Arc<dyn WebBackend>, String>,
    extract: Result<Arc<dyn WebBackend>, String>,
    fetcher: Arc<Fetcher>,
    full_text: FullTextStore,
    config: WebConfig,
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
        Self::from_config_with(config, client, fetcher, secrets, full_text, &|_, _| None)
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
        let has_secret = |name: &str| secrets(name).is_some();
        let build = |capability| {
            let id = select::configured(config, capability);
            if id != "auto"
                && !BACKENDS.contains(&id)
                && let Some(plugin) = plugins(capability, id)
            {
                return Ok(plugin);
            }
            resolve(config, capability, &has_secret).map(|id| backend(id, config, client, secrets))
        };
        Self {
            search: build(Capability::Search),
            extract: build(Capability::Extract),
            fetcher,
            full_text,
            config: config.clone(),
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
            search: search.ok_or_else(missing),
            extract: extract.ok_or_else(missing),
            fetcher,
            full_text,
            config: config.clone(),
        }
    }

    /// The `web_extract` timeout per URL (`None`: disabled).
    fn extract_timeout(&self) -> Option<Duration> {
        let secs = self.config.extract_timeout_secs;
        (secs > 0).then(|| Duration::from_secs(secs))
    }
}

fn backend(
    id: &str,
    config: &WebConfig,
    client: &reqwest::Client,
    secrets: &SecretLookup,
) -> Arc<dyn WebBackend> {
    if id == "searxng" {
        let url = config.searxng.url.as_deref().unwrap_or_default();
        return Arc::new(Searxng::new(client.clone(), url));
    }
    Arc::new(Firecrawl::new(
        client.clone(),
        config.firecrawl.base_url.as_deref(),
        Arc::clone(secrets),
    ))
}
