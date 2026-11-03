//! Provider objects registered by `provider`-world plugins (spec:
//! plugin-system, Provider-object plugins fail closed when unregistered).
//!
//! Image and video generation, web search and extract, text-to-speech,
//! transcription and secret sources are selected by name in config
//! (`image_gen.provider`, `video_gen.provider`, `web.search_backend`,
//! `web.extract_backend`, `tts.provider`, `stt.provider`,
//! `secrets.sources`). Every category resolves a name the same way: a
//! built-in of the category first, then a provider an enabled plugin
//! registered, and otherwise the feature is unavailable with
//! `<category> provider '<name>' is not installed or not enabled`, never
//! another provider in its place. The feature that owns a key calls
//! [`ProviderRegistry::require`] (web search and extract do through
//! [`ProviderRegistry::web_backend`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_tools::web::backend::{Hit, Page, WebBackend, WebFuture};
use serde_json::{Value, json};

use crate::host::provider_world::Category as WitCategory;
use crate::host::{CallInfo, Instance};

/// A provider category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// `image_gen.provider`.
    ImageGen,
    /// `video_gen.provider`.
    VideoGen,
    /// `web.search_backend`.
    WebSearch,
    /// `web.extract_backend`.
    WebExtract,
    /// `tts.provider`.
    Tts,
    /// `stt.provider`.
    Stt,
    /// `secrets.sources`.
    SecretSource,
}

impl Category {
    /// The name errors use (`image_gen provider 'fal' ...`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageGen => "image_gen",
            Self::VideoGen => "video_gen",
            Self::WebSearch => "web_search",
            Self::WebExtract => "web_extract",
            Self::Tts => "tts",
            Self::Stt => "stt",
            Self::SecretSource => "secret_source",
        }
    }

    fn of(category: WitCategory) -> Self {
        match category {
            WitCategory::ImageGen => Self::ImageGen,
            WitCategory::VideoGen => Self::VideoGen,
            WitCategory::WebSearch => Self::WebSearch,
            WitCategory::WebExtract => Self::WebExtract,
            WitCategory::Tts => Self::Tts,
            WitCategory::Stt => Self::Stt,
            WitCategory::SecretSource => Self::SecretSource,
        }
    }
}

/// The error of a name nothing provides.
#[must_use]
pub fn not_installed(category: Category, name: &str) -> Error {
    Error::new(
        &codes::PLUGINS_NOT_FOUND,
        format!(
            "{} provider '{name}' is not installed or not enabled",
            category.as_str()
        ),
    )
}

/// One provider a plugin registered.
#[derive(Debug, Clone)]
pub struct PluginProvider {
    /// The registering plugin.
    pub plugin: String,
    /// The name config selects.
    pub id: String,
    /// Its category.
    pub category: Category,
    /// One line for listings.
    pub description: String,
    instance: Arc<Instance>,
}

impl PluginProvider {
    /// Sends one JSON request document and returns the JSON answer
    /// (blocking: call it off the async runtime).
    ///
    /// # Errors
    ///
    /// The plugin's error text, or the trap or timeout.
    pub fn invoke(&self, call: &CallInfo, request: &str) -> Result<String, String> {
        match self.instance.invoke_provider(call, &self.id, request) {
            Ok(answer) => answer,
            Err(err) => Err(err.message().to_owned()),
        }
    }
}

/// The providers of one load.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    entries: BTreeMap<(Category, String), PluginProvider>,
}

impl ProviderRegistry {
    /// Registers what `instance`'s `providers()` answered; returns the
    /// warnings of refused entries (a name another plugin took first).
    pub fn add(
        &mut self,
        instance: &Arc<Instance>,
        specs: Vec<crate::host::provider_world::ProviderSpec>,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        for spec in specs {
            let category = Category::of(spec.category);
            let key = (category, spec.id.clone());
            if let Some(owner) = self.entries.get(&key) {
                warnings.push(format!(
                    "{} provider '{}' is already registered by plugin '{}'",
                    category.as_str(),
                    spec.id,
                    owner.plugin
                ));
                continue;
            }
            self.entries.insert(
                key,
                PluginProvider {
                    plugin: instance.id().to_owned(),
                    id: spec.id,
                    category,
                    description: spec.description,
                    instance: Arc::clone(instance),
                },
            );
        }
        warnings
    }

    /// Every registered provider, by category then name.
    pub fn iter(&self) -> impl Iterator<Item = &PluginProvider> {
        self.entries.values()
    }

    /// The plugin provider `name` of `category`.
    ///
    /// # Errors
    ///
    /// `plugins.not_found`: `<category> provider '<name>' is not installed
    /// or not enabled`.
    pub fn require(&self, category: Category, name: &str) -> Result<&PluginProvider, Error> {
        self.entries
            .get(&(category, name.to_owned()))
            .ok_or_else(|| not_installed(category, name))
    }

    /// `name` as a web backend: search, plus extract when the same plugin
    /// also registered `name` as an extract provider.
    ///
    /// # Errors
    ///
    /// As [`Self::require`] for `category`.
    pub fn web_backend(
        &self,
        category: Category,
        name: &str,
        call: &CallInfo,
    ) -> Result<Arc<dyn WebBackend>, Error> {
        let primary = self.require(category, name)?.clone();
        let search = self
            .entries
            .get(&(Category::WebSearch, name.to_owned()))
            .cloned();
        let extract = self
            .entries
            .get(&(Category::WebExtract, name.to_owned()))
            .cloned();
        Ok(Arc::new(PluginWeb {
            id: primary.id,
            search,
            extract,
            call: call.clone(),
        }))
    }
}

/// A plugin's web search and extract providers as a [`WebBackend`].
/// Requests are `{"query", "limit"}` (answer `{"results": [{"title",
/// "url", "snippet"}]}`) and `{"url"}` (answer `{"title", "markdown"}`).
struct PluginWeb {
    id: String,
    search: Option<PluginProvider>,
    extract: Option<PluginProvider>,
    call: CallInfo,
}

impl PluginWeb {
    fn invoke<'a>(
        &'a self,
        provider: Option<&'a PluginProvider>,
        category: Category,
        request: Value,
    ) -> WebFuture<'a, Value> {
        Box::pin(async move {
            let provider = provider
                .ok_or_else(|| not_installed(category, &self.id).message().to_owned())?
                .clone();
            let call = self.call.clone();
            let answer =
                tokio::task::spawn_blocking(move || provider.invoke(&call, &request.to_string()))
                    .await
                    .map_err(|e| e.to_string())??;
            serde_json::from_str(&answer)
                .map_err(|e| format!("provider '{}' answered invalid JSON: {e}", self.id))
        })
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl WebBackend for PluginWeb {
    fn id(&self) -> &str {
        &self.id
    }

    fn can_extract(&self) -> bool {
        self.extract.is_some()
    }

    fn search<'a>(&'a self, query: &'a str, limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let request = json!({ "query": query, "limit": limit });
            let answer = self
                .invoke(self.search.as_ref(), Category::WebSearch, request)
                .await?;
            Ok(answer
                .get("results")
                .and_then(Value::as_array)
                .map(|hits| {
                    hits.iter()
                        .map(|h| Hit {
                            title: text(h, "title"),
                            url: text(h, "url"),
                            snippet: text(h, "snippet"),
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn extract<'a>(&'a self, url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async move {
            let answer = self
                .invoke(
                    self.extract.as_ref(),
                    Category::WebExtract,
                    json!({ "url": url }),
                )
                .await?;
            Ok(Page {
                title: text(&answer, "title"),
                markdown: text(&answer, "markdown"),
            })
        })
    }
}
