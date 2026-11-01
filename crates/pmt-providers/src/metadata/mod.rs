//! The model metadata catalog (spec: model-providers, Model metadata
//! catalog; Context length detection chain): context window, output cap,
//! capabilities and prices of a model, field by field from
//! `[models."<provider>/<model>"]` overrides, then the provider's cached
//! live listing, then the built-in catalog, then defaults (tools, no
//! vision, unknown prices) with one WARN per (provider, model) per process
//! naming the override key. The context window follows its own longer
//! chain (see [`window`]). Lookups never touch the network;
//! [`Catalog::refresh`] fetches a listing when its cache is stale.

mod builtin;
pub mod listing;
pub mod modelsdev;
pub mod window;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use jiff::Timestamp;
use pmt_config::Config;
use pmt_config::schema::{ModelEntry, ModelRoute, ReasoningLevel};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_obs::usage::{Price, Prices};
use pmt_store::Store;
use pmt_store::model_context::{DiscoveredContext, Discovery};
use serde::Serialize;

use crate::registry::Provider;
use listing::{Listed, Listing};
use modelsdev::ModelsDev;
pub use window::ContextSource;

/// Context window assumed for a model nothing describes.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// What is known about one model (the capability flags are the
/// catalog's own fields).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelMetadata {
    /// Context window in tokens.
    pub context_window: u64,
    /// Output cap, when known.
    pub max_output_tokens: Option<u64>,
    /// Takes tool schemas.
    pub supports_tools: bool,
    /// Takes images.
    pub supports_vision: bool,
    /// Takes video.
    pub supports_video: bool,
    /// Reasons (thinking tokens).
    pub supports_reasoning: bool,
    /// Highest reasoning effort accepted, when known.
    pub max_reasoning_effort: Option<ReasoningLevel>,
    /// Where the context window came from.
    pub context_source: ContextSource,
    /// Per-million prices, when known.
    #[serde(skip)]
    pub price: Option<Price>,
}

impl Default for ModelMetadata {
    fn default() -> Self {
        Self {
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: None,
            supports_tools: true,
            supports_vision: false,
            supports_video: false,
            supports_reasoning: false,
            max_reasoning_effort: None,
            context_source: ContextSource::Default,
            price: None,
        }
    }
}

/// Which sources described a model (the most authoritative one first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    /// A config override.
    Override,
    /// The provider's live listing.
    Listing,
    /// The built-in catalog.
    Builtin,
    /// Nothing: defaults.
    Default,
}

/// One profile's model catalog.
#[derive(Debug)]
pub struct Catalog {
    overrides: BTreeMap<String, ModelRoute>,
    cache_dir: Option<PathBuf>,
    listings: Mutex<BTreeMap<String, Listing>>,
    /// Windows a local server probe reported this session.
    probes: Mutex<BTreeMap<String, u64>>,
    /// Discovered-window cache (`state.db`).
    store: Option<Arc<Store>>,
    /// `providers.<id>.catalog_ttl_secs` of configured providers.
    ttls: BTreeMap<String, u64>,
    models_dev: Mutex<Option<Arc<ModelsDev>>>,
}

impl Catalog {
    /// The catalog of `config`; listings are cached under `cache_dir`
    /// (the profile's `cache/models`), or only in memory when `None`.
    #[must_use]
    pub fn new(config: &Config, cache_dir: Option<PathBuf>) -> Self {
        let mut overrides = BTreeMap::new();
        for (name, entry) in &config.models {
            let ModelEntry::Route(route) = entry else {
                continue;
            };
            if let (Some(provider), Some(model)) = (&route.provider, &route.model) {
                overrides.insert(format!("{provider}/{model}"), route.clone());
            }
            // The entry named `<provider>/<model>` wins over a route naming it.
            overrides.insert(name.clone(), route.clone());
        }
        Self {
            overrides,
            cache_dir,
            listings: Mutex::new(BTreeMap::new()),
            probes: Mutex::new(BTreeMap::new()),
            store: None,
            ttls: config
                .providers
                .iter()
                .map(|(id, p)| (id.clone(), p.catalog_ttl_secs))
                .collect(),
            models_dev: Mutex::new(None),
        }
    }

    /// Keeps discovered windows in (and reads them from) `store`.
    #[must_use]
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// The cached listing of `provider` (memory, then disk), fresh or not.
    fn listing(&self, provider: &str) -> Option<Listing> {
        let mut listings = self.listings.lock().expect("catalog lock");
        if let Some(listing) = listings.get(provider) {
            return Some(listing.clone());
        }
        let listing = listing::read(self.cache_dir.as_deref()?, provider)?;
        listings.insert(provider.to_owned(), listing.clone());
        Some(listing)
    }

    fn listed(&self, provider: &str, model: &str) -> Option<Listed> {
        self.listing(provider)?
            .models
            .into_iter()
            .find(|m| m.id == model)
    }

    /// The metadata of `provider`/`model` without an endpoint (the
    /// discovered-window cache is keyed by base URL, so it is skipped).
    #[must_use]
    pub fn metadata(&self, provider: &str, model: &str) -> (ModelMetadata, Vec<MetadataSource>) {
        self.metadata_at(provider, model, None)
    }

    /// The metadata of `provider`/`model` served at `base_url` and the
    /// sources that described its fields. Never makes a network request.
    #[must_use]
    pub fn metadata_at(
        &self,
        provider: &str,
        model: &str,
        base_url: Option<&str>,
    ) -> (ModelMetadata, Vec<MetadataSource>) {
        let mut meta = ModelMetadata::default();
        let mut sources = Vec::new();
        let shipped = builtin::lookup(provider, model);
        if let Some(shipped) = &shipped {
            meta = shipped.clone();
            sources.push(MetadataSource::Builtin);
        }
        let listed = self.listed(provider, model);
        if let Some(listed) = &listed {
            apply_listed(&mut meta, listed);
            sources.push(MetadataSource::Listing);
        }
        let route = self.overrides.get(&format!("{provider}/{model}"));
        if let Some(route) = route {
            apply_override(&mut meta, route);
            sources.push(MetadataSource::Override);
        }
        sources.reverse();
        let found = window::Found {
            pinned: route.and_then(|r| r.context_window),
            listed: listed.and_then(|l| l.context_window),
            probed: self.probed(provider, model),
            discovered: base_url.and_then(|url| self.discovered(provider, model, url)),
            builtin: shipped.map(|m| m.context_window),
        };
        let (context_window, context_source) = window::resolve(
            &found,
            || self.models_dev().context_window(provider, model),
            model,
        );
        meta.context_window = context_window;
        meta.context_source = context_source;
        if sources.is_empty() {
            if context_source.is_guess() {
                warn_once(provider, model);
            }
            sources.push(MetadataSource::Default);
        }
        (meta, sources)
    }

    /// Records the window a local server probe reported for `model` at
    /// `base_url` (this session's catalog and the discovered cache).
    ///
    /// # Panics
    ///
    /// Only if another thread panicked while holding the catalog lock.
    pub fn record_probe(&self, provider: &str, model: &str, base_url: &str, window: u64) {
        self.probes
            .lock()
            .expect("catalog lock")
            .insert(format!("{provider}/{model}"), window);
        self.remember(&[DiscoveredContext {
            provider: provider.to_owned(),
            model: model.to_owned(),
            base_url: base_url.to_owned(),
            context_window: window,
            source: Discovery::Probe,
        }]);
    }

    fn probed(&self, provider: &str, model: &str) -> Option<u64> {
        self.probes
            .lock()
            .expect("catalog lock")
            .get(&format!("{provider}/{model}"))
            .copied()
    }

    /// A discovered window still within the provider's catalog TTL.
    fn discovered(&self, provider: &str, model: &str, base_url: &str) -> Option<u64> {
        let store = self.store.as_ref()?;
        let (window, at) = store
            .discovered_context(provider, model, base_url)
            .inspect_err(|err| tracing::warn!("cannot read the discovered context cache: {err}"))
            .ok()??;
        let ttl = self
            .ttls
            .get(provider)
            .copied()
            .unwrap_or_else(|| pmt_config::schema::ProviderConfig::default().catalog_ttl_secs);
        let age = Timestamp::now().as_second().saturating_sub(at.as_second());
        u64::try_from(age)
            .is_ok_and(|age| age < ttl)
            .then_some(window)
    }

    fn remember(&self, found: &[DiscoveredContext]) {
        if let Some(store) = &self.store
            && let Err(err) = store.record_discovered_contexts(found, Timestamp::now())
        {
            tracing::warn!("cannot cache discovered context windows: {err}");
        }
    }

    /// The models.dev snapshot: the one `model list --refresh` cached, else
    /// the one bundled with the binary.
    fn models_dev(&self) -> Arc<ModelsDev> {
        let mut slot = self.models_dev.lock().expect("catalog lock");
        slot.get_or_insert_with(|| Arc::new(ModelsDev::load(self.cache_dir.as_deref())))
            .clone()
    }

    /// Fetches the models.dev catalog from `url` and caches it for later
    /// processes (`prometheus model list --refresh`).
    ///
    /// # Errors
    ///
    /// `provider.call_failed` when the download or its parse fails;
    /// `home.io` when it cannot be cached.
    ///
    /// # Panics
    ///
    /// Only if another thread panicked while holding the catalog lock.
    pub async fn refresh_models_dev(
        &self,
        client: &reqwest::Client,
        url: &str,
    ) -> Result<(), Error> {
        let snapshot = modelsdev::fetch(client, url).await?;
        if let Some(dir) = &self.cache_dir {
            modelsdev::write(dir, &snapshot)?;
        }
        *self.models_dev.lock().expect("catalog lock") = Some(Arc::new(snapshot));
        Ok(())
    }

    /// Models to offer in a picker: the live listing when the provider
    /// lists models and one is cached, else the built-in entries.
    #[must_use]
    pub fn picker_models(&self, provider: &Provider) -> Vec<String> {
        if provider.supports_model_listing()
            && let Some(listing) = self.listing(provider.id())
        {
            return listing.models.into_iter().map(|m| m.id).collect();
        }
        builtin::models(provider.id())
    }

    /// Fetches `provider`'s listing unless it does not list models or its
    /// cached listing is younger than `providers.<id>.catalog_ttl_secs`.
    /// Returns whether a request was made.
    ///
    /// # Errors
    ///
    /// `provider.call_failed` / `provider.auth_failed` when the listing
    /// request fails (callers treat a listing as best effort).
    ///
    /// # Panics
    ///
    /// Only if another thread panicked while holding the catalog lock.
    pub async fn refresh(
        &self,
        client: &reqwest::Client,
        provider: &Provider,
        credential: Option<&SecretString>,
        now: Timestamp,
    ) -> Result<bool, Error> {
        if !provider.supports_model_listing() {
            return Ok(false);
        }
        let ttl = provider.settings().catalog_ttl_secs;
        if self
            .listing(provider.id())
            .is_some_and(|l| l.is_fresh(ttl, now))
        {
            return Ok(false);
        }
        let Some(url) = provider.models_url() else {
            return Ok(false);
        };
        let mut request = client.get(&url);
        if let Some(secret) = credential {
            request = request.bearer_auth(secret.expose());
        }
        let failed = |e: &dyn std::fmt::Display| {
            Error::new(
                &codes::PROVIDER_CALL_FAILED,
                format!("{}: model listing failed: {e}", provider.id()),
            )
        };
        let response = request.send().await.map_err(|e| failed(&e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| failed(&e))?;
        if !status.is_success() {
            let error = crate::classify::from_response(
                status.as_u16(),
                &reqwest::header::HeaderMap::new(),
                &text,
                now,
            );
            return Err(error.to_error(provider.id()));
        }
        let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| failed(&e))?;
        let listing = Listing {
            fetched_at: now.as_second(),
            models: listing::parse(&body),
        };
        if let Some(base_url) = provider.base_url() {
            let found: Vec<DiscoveredContext> = listing
                .models
                .iter()
                .filter_map(|m| {
                    Some(DiscoveredContext {
                        provider: provider.id().to_owned(),
                        model: m.id.clone(),
                        base_url: base_url.to_owned(),
                        context_window: m.context_window?,
                        source: Discovery::Listing,
                    })
                })
                .collect();
            self.remember(&found);
        }
        if let Some(dir) = &self.cache_dir
            && let Err(err) = listing::write(dir, provider.id(), &listing)
        {
            tracing::warn!(
                provider = provider.id(),
                "cannot cache the model listing: {err}"
            );
        }
        self.listings
            .lock()
            .expect("catalog lock")
            .insert(provider.id().to_owned(), listing);
        Ok(true)
    }
}

impl Prices for Catalog {
    fn price(&self, provider: &str, model: &str) -> Option<Price> {
        let override_price = self
            .overrides
            .get(&format!("{provider}/{model}"))
            .and_then(|r| r.price.as_ref())
            .map(Price::from);
        override_price
            .or_else(|| self.listed(provider, model).and_then(|l| l.price()))
            .or_else(|| builtin::lookup(provider, model).and_then(|m| m.price))
    }
}

fn apply_listed(meta: &mut ModelMetadata, listed: &Listed) {
    if listed.max_output_tokens.is_some() {
        meta.max_output_tokens = listed.max_output_tokens;
    }
    if let Some(tools) = listed.supports_tools {
        meta.supports_tools = tools;
    }
    if let Some(vision) = listed.supports_vision {
        meta.supports_vision = vision;
    }
    if let Some(price) = listed.price() {
        meta.price = Some(price);
    }
}

fn apply_override(meta: &mut ModelMetadata, route: &ModelRoute) {
    let set = |slot: &mut bool, value: Option<bool>| {
        if let Some(v) = value {
            *slot = v;
        }
    };
    if route.max_output_tokens.is_some() {
        meta.max_output_tokens = route.max_output_tokens;
    }
    set(&mut meta.supports_tools, route.supports_tools);
    set(&mut meta.supports_vision, route.supports_vision);
    set(&mut meta.supports_video, route.supports_video);
    set(&mut meta.supports_reasoning, route.supports_reasoning);
    if route.max_reasoning_effort.is_some() {
        meta.max_reasoning_effort = route.max_reasoning_effort;
    }
    if let Some(price) = &route.price {
        meta.price = Some(Price::from(price));
    }
}

fn warn_once(provider: &str, model: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let key = format!("{provider}/{model}");
    let first = WARNED
        .get_or_init(Mutex::default)
        .lock()
        .expect("warned lock")
        .insert(key.clone());
    if first {
        tracing::warn!(
            provider,
            model,
            "no metadata for model '{key}': assuming a {DEFAULT_CONTEXT_WINDOW}-token context window, tool support, no vision and unknown prices; set [models.\"{key}\"] to override"
        );
    }
}

#[cfg(test)]
mod tests;
