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
pub mod tools;

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::WebConfig;
use pmt_net::fetch::Fetcher;

pub use backend::{Firecrawl, Hit, Page, Searxng, SecretLookup, WebBackend};
pub use tools::tools;

/// Backend ids this build implements.
pub const BACKENDS: &[&str] = &["firecrawl", "searxng"];

/// Which web capability a backend serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `web_search`.
    Search,
    /// `web_extract`.
    Extract,
}

/// The backend id configured for `capability`, or why there is none. This
/// is also the tools' availability probe.
///
/// # Errors
///
/// The unavailability reason (`no web backend configured`, a search-only
/// backend chosen for extract, an id this build does not implement).
pub fn configured(config: &WebConfig, capability: Capability) -> Result<&str, String> {
    let specific = match capability {
        Capability::Search => config.search_backend.as_deref(),
        Capability::Extract => config.extract_backend.as_deref(),
    };
    let id = specific.unwrap_or(&config.backend);
    if id == "auto" {
        return Err("no web backend configured".to_owned());
    }
    if !BACKENDS.contains(&id) {
        return Err(format!("web backend '{id}' is not available in this build"));
    }
    if capability == Capability::Extract && id == "searxng" {
        return Err(format!(
            "backend '{id}' is search-only; set web.extract_backend"
        ));
    }
    if id == "searxng" && config.searxng.url.is_none() {
        return Err("backend 'searxng' needs web.searxng.url".to_owned());
    }
    Ok(id)
}

/// One session's web tools.
pub struct WebSession {
    search: Result<Arc<dyn WebBackend>, String>,
    extract: Result<Arc<dyn WebBackend>, String>,
    fetcher: Arc<Fetcher>,
    config: WebConfig,
}

impl WebSession {
    /// The session's backends from `config`. `client` calls the backends'
    /// APIs; `fetcher` carries the session's destination policy.
    #[must_use]
    pub fn from_config(
        config: &WebConfig,
        client: &reqwest::Client,
        fetcher: Arc<Fetcher>,
        secrets: &SecretLookup,
    ) -> Self {
        let build = |capability| {
            configured(config, capability).map(|id| backend(id, config, client, secrets))
        };
        Self {
            search: build(Capability::Search),
            extract: build(Capability::Extract),
            fetcher,
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
    ) -> Self {
        let missing = || "no web backend configured".to_owned();
        Self {
            search: search.ok_or_else(missing),
            extract: extract.ok_or_else(missing),
            fetcher,
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

#[cfg(test)]
mod selection_tests {
    use pmt_config::schema::WebConfig;

    use super::{Capability, configured};

    #[test]
    fn per_capability_backends_and_reasons() {
        let mut config = WebConfig::default();
        assert_eq!(
            configured(&config, Capability::Search),
            Err("no web backend configured".into())
        );
        config.search_backend = Some("searxng".into());
        config.searxng.url = Some("http://127.0.0.1:8888".into());
        config.extract_backend = Some("firecrawl".into());
        assert_eq!(configured(&config, Capability::Search), Ok("searxng"));
        assert_eq!(configured(&config, Capability::Extract), Ok("firecrawl"));
        config.extract_backend = None;
        config.backend = "searxng".into();
        assert_eq!(
            configured(&config, Capability::Extract),
            Err("backend 'searxng' is search-only; set web.extract_backend".into())
        );
        config.backend = "tavily".into();
        assert!(
            configured(&config, Capability::Extract)
                .unwrap_err()
                .contains("tavily")
        );
    }
}
