//! The plugin provider registry as the media tools see it (spec:
//! plugin-system, Provider-object plugins fail closed when unregistered):
//! `image_gen.provider`, `tts.provider` and `stt.provider` names that are
//! not built in resolve through [`ProviderRegistry::require`].

use std::sync::Arc;

use pmt_media::plugin::{Kind, MediaPlugins, PluginFuture, SttFuture, SttRequest};
use pmt_plugins::hooks::{Hooks, Payload};
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::Reply;
use pmt_plugins::load::Loaded;
use pmt_plugins::providers::{Category, ProviderRegistry};
use serde_json::{Map, Value};

/// The providers and hooks of one plugin load, called for one session
/// (or surface).
pub struct PluginMedia {
    providers: ProviderRegistry,
    hooks: Arc<Hooks>,
    call: CallInfo,
}

impl PluginMedia {
    /// The media providers and hooks of `loaded`, called as `call`.
    #[must_use]
    pub fn shared(loaded: &Loaded, call: CallInfo) -> Arc<dyn MediaPlugins> {
        Arc::new(Self {
            providers: loaded.providers.clone(),
            hooks: Arc::clone(&loaded.hooks),
            call,
        })
    }
}

fn category(kind: Kind) -> Category {
    match kind {
        Kind::ImageGen => Category::ImageGen,
        Kind::Tts => Category::Tts,
        Kind::Stt => Category::Stt,
    }
}

impl MediaPlugins for PluginMedia {
    fn plugin_of(&self, kind: Kind, id: &str) -> Option<String> {
        self.providers
            .require(category(kind), id)
            .ok()
            .map(|p| p.plugin.clone())
    }

    fn invoke<'a>(&'a self, kind: Kind, id: &'a str, request: Value) -> PluginFuture<'a> {
        Box::pin(async move {
            let provider = self
                .providers
                .require(category(kind), id)
                .map_err(|e| e.message().to_owned())?
                .clone();
            let call = self.call.clone();
            // A plugin call blocks (a WASM instance or a sidecar round trip).
            let answer =
                pmt_core::spawn::blocking(move || provider.invoke(&call, &request.to_string()))
                    .await
                    .map_err(|e| e.to_string())??;
            serde_json::from_str(&answer)
                .map_err(|e| format!("provider '{id}' answered invalid JSON: {e}"))
        })
    }

    fn pre_transcription(&self, mut request: SttRequest) -> SttFuture<'_> {
        Box::pin(async move {
            if !self.hooks.handles("pre_transcription") {
                return request;
            }
            let mut fields = Map::new();
            fields.insert("provider".to_owned(), request.provider.clone().into());
            fields.insert("model".to_owned(), request.model.clone().into());
            fields.insert("language".to_owned(), request.language.clone().into());
            fields.insert("prompt".to_owned(), request.prompt.clone().into());
            let answers = self
                .hooks
                .fire("pre_transcription", &Payload::new(fields), &self.call)
                .await;
            for answer in answers {
                if let Reply::Modify(text) = &answer.reply
                    && let Ok(table) = serde_json::from_str::<Value>(text)
                {
                    request.apply(&table);
                }
            }
            request
        })
    }
}
