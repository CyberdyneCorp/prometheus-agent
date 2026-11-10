//! The plugin provider registry as the media tools see it (spec:
//! plugin-system, Provider-object plugins fail closed when unregistered):
//! `image_gen.provider`, `tts.provider` and `stt.provider` names that are
//! not built in resolve through [`ProviderRegistry::require`].

use std::sync::Arc;

use pmt_media::plugin::{Kind, MediaPlugins, PluginFuture};
use pmt_plugins::host::CallInfo;
use pmt_plugins::providers::{Category, ProviderRegistry};
use serde_json::Value;

/// The providers of one plugin load, called for one session (or surface).
pub struct PluginMedia {
    providers: ProviderRegistry,
    call: CallInfo,
}

impl PluginMedia {
    /// The media providers of `providers`, called as `call`.
    #[must_use]
    pub fn shared(providers: ProviderRegistry, call: CallInfo) -> Arc<dyn MediaPlugins> {
        Arc::new(Self { providers, call })
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
                tokio::task::spawn_blocking(move || provider.invoke(&call, &request.to_string()))
                    .await
                    .map_err(|e| e.to_string())??;
            serde_json::from_str(&answer)
                .map_err(|e| format!("provider '{id}' answered invalid JSON: {e}"))
        })
    }
}
