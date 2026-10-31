//! Wire formats (spec: model-providers, Wire formats, selection and
//! normalised event stream): selection for a route, request encoding and
//! stream decoding of the three native formats.

mod anthropic;
mod chat;
mod responses;

pub(crate) use anthropic::API_VERSION as ANTHROPIC_VERSION;
pub(crate) use anthropic::OAUTH_BETA as ANTHROPIC_OAUTH_BETA;

use pmt_config::provider_urls::host_of;
use pmt_config::schema::WireFormat;
use serde::Serialize;
use serde_json::Value;

use pmt_core::error::{Error, codes};

use crate::classify::ProviderError;
use crate::descriptor::WireNeed;
use crate::event::Normalizer;
use crate::registry::Provider;
use crate::request::Request;
use crate::sse::SseEvent;

/// Which rule chose the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireSource {
    /// `providers.<id>.wire_format`.
    Config,
    /// A host rule matched the base URL.
    HostRule,
    /// The descriptor's declared format.
    Descriptor,
    /// Chat Completions, the format of OpenAI-compatible relays.
    Default,
}

/// Selects the wire format of `provider` at `base_url`: explicit config,
/// then host rules, then the declared format (Anthropic Messages only when
/// `base_url` is on the descriptor's own host), else Chat Completions.
#[must_use]
pub fn select(provider: &Provider, base_url: &str) -> (WireFormat, WireSource) {
    if let Some(format) = provider.settings().wire_format {
        return (format, WireSource::Config);
    }
    if let Some(rule) = provider.host_rules().iter().find(|r| r.matches(base_url)) {
        return (rule.wire_format, WireSource::HostRule);
    }
    let declared = provider.descriptor().wire_format;
    if declared != WireFormat::AnthropicMessages {
        return (declared, WireSource::Descriptor);
    }
    let own_host = provider.descriptor().base_url.as_deref().and_then(host_of);
    if own_host.is_some() && own_host == host_of(base_url) {
        (declared, WireSource::Descriptor)
    } else {
        (WireFormat::ChatCompletions, WireSource::Default)
    }
}

/// Why `provider` cannot be routed with the format `select` chose: its
/// declared format is a plugin's and neither config nor a host rule chose
/// a native one (`provider.plugin_unavailable`).
#[must_use]
pub fn available(provider: &Provider, source: WireSource) -> Option<Error> {
    let WireNeed::Plugin { wire, plugin, hint } = &provider.descriptor().wire_need else {
        return None;
    };
    if matches!(source, WireSource::Config | WireSource::HostRule) {
        return None;
    }
    Some(
        Error::new(
            &codes::PROVIDER_PLUGIN_UNAVAILABLE,
            format!(
                "provider '{}' speaks {wire}, which the provider plugin '{plugin}' implements; that plugin is not installed",
                provider.id()
            ),
        )
        .with_hint(hint.clone()),
    )
}

/// Why a virtual provider (served in-process by another capability)
/// cannot be routed in this build.
#[must_use]
pub fn virtual_unavailable(provider: &Provider) -> Option<Error> {
    let WireNeed::Virtual { owner, .. } = &provider.descriptor().wire_need else {
        return None;
    };
    Some(Error::new(
        &codes::PROVIDER_PLUGIN_UNAVAILABLE,
        format!(
            "provider '{}' is the virtual provider of {owner}, which this build does not include yet",
            provider.id()
        ),
    ))
}

/// Whether `provider` can be routed at its effective endpoint (auto
/// detection passes over the ones that cannot).
#[must_use]
pub fn routable(provider: &Provider) -> bool {
    if virtual_unavailable(provider).is_some() {
        return false;
    }
    provider
        .base_url()
        .is_none_or(|url| available(provider, select(provider, url).1).is_none())
}

/// Per-request encoding settings that are not part of the request.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EncodeOptions<'a> {
    /// The prompt cache key to send (`responses`/`chat_completions`
    /// providers that declare `supports_prompt_cache`).
    pub cache_key: Option<&'a str>,
}

/// The request body for `format`.
pub(crate) fn encode(format: WireFormat, request: &Request, options: &EncodeOptions<'_>) -> Value {
    match format {
        WireFormat::ChatCompletions => chat::encode(request, options),
        WireFormat::Responses => responses::encode(request, options),
        WireFormat::AnthropicMessages => anthropic::encode(request),
    }
}

/// The endpoint URL of `format` under `base_url`.
pub(crate) fn endpoint(format: WireFormat, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match format {
        WireFormat::ChatCompletions => format!("{base}/chat/completions"),
        WireFormat::Responses => format!("{base}/responses"),
        WireFormat::AnthropicMessages if base.ends_with("/v1") => format!("{base}/messages"),
        WireFormat::AnthropicMessages => format!("{base}/v1/messages"),
    }
}

/// Decodes one wire format's SSE events into the normaliser.
pub(crate) trait Decode: Send {
    /// One event.
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError>;
    /// The stream ended (EOF) before the normaliser finished.
    fn end(&mut self, out: &mut Normalizer) -> Result<(), ProviderError>;
}

/// A fresh decoder for `format`.
pub(crate) fn decoder(format: WireFormat) -> Box<dyn Decode> {
    match format {
        WireFormat::ChatCompletions => Box::<chat::Decoder>::default(),
        WireFormat::Responses => Box::<responses::Decoder>::default(),
        WireFormat::AnthropicMessages => Box::<anthropic::Decoder>::default(),
    }
}

/// Parses an event's JSON data.
fn json(event: &SseEvent) -> Result<Value, ProviderError> {
    serde_json::from_str(&event.data).map_err(|e| malformed(&e.to_string()))
}

/// A stream event that cannot be decoded.
pub(crate) fn malformed(detail: &str) -> ProviderError {
    ProviderError::new(
        crate::classify::ErrorClass::ServerError,
        format!("malformed stream event: {detail}"),
    )
}

/// The stream ended without its terminal event.
pub(crate) fn truncated() -> ProviderError {
    ProviderError::new(
        crate::classify::ErrorClass::Network,
        "the stream ended before the response was complete",
    )
}

/// A `u64` at `pointer`, if present.
fn count(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

/// The whole system prompt as one text (formats without system blocks).
fn system_text(request: &Request) -> Option<String> {
    (!request.system.is_empty()).then(|| {
        request
            .system
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    })
}

/// A data URL (or the URL itself) for an image part.
fn image_url(media_type: &str, data: &str, is_url: bool) -> String {
    if is_url {
        data.to_owned()
    } else {
        format!("data:{media_type};base64,{data}")
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::Config;

    use super::*;
    use crate::registry::ProviderRegistry;

    fn registry(text: &str) -> ProviderRegistry {
        ProviderRegistry::for_profile(&toml::from_str::<Config>(text).unwrap(), Vec::new()).unwrap()
    }

    #[test]
    fn anthropic_declared_provider_behind_a_foreign_relay_speaks_chat_completions() {
        let r = registry("[providers.anthropic]\nbase_url = \"https://relay.example.net/v1\"\n");
        let p = r.lookup("anthropic").unwrap();
        assert_eq!(
            select(p, p.base_url().unwrap()),
            (WireFormat::ChatCompletions, WireSource::Default)
        );
        let own = registry("");
        let p = own.lookup("anthropic").unwrap();
        assert_eq!(
            select(p, p.base_url().unwrap()),
            (WireFormat::AnthropicMessages, WireSource::Descriptor)
        );
    }

    #[test]
    fn selection_order_is_config_then_host_rule_then_descriptor() {
        let r = registry(
            "[providers.relay]\nbase_url = \"https://relay.example.net/anthropic\"\n\
             [providers.forced]\nbase_url = \"https://relay.example.net/anthropic\"\nwire_format = \"responses\"\n",
        );
        let relay = r.lookup("relay").unwrap();
        assert_eq!(
            select(relay, relay.base_url().unwrap()),
            (WireFormat::AnthropicMessages, WireSource::HostRule)
        );
        let forced = r.lookup("forced").unwrap();
        assert_eq!(
            select(forced, forced.base_url().unwrap()),
            (WireFormat::Responses, WireSource::Config)
        );
        let or = r.lookup("openrouter").unwrap();
        assert_eq!(
            select(or, or.base_url().unwrap()),
            (WireFormat::ChatCompletions, WireSource::Descriptor)
        );
    }

    #[test]
    fn endpoints_per_format() {
        assert_eq!(
            endpoint(WireFormat::AnthropicMessages, "https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            endpoint(
                WireFormat::AnthropicMessages,
                "https://x.test/anthropic/v1/"
            ),
            "https://x.test/anthropic/v1/messages"
        );
        assert_eq!(
            endpoint(WireFormat::ChatCompletions, "https://x.test/v1/"),
            "https://x.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint(WireFormat::Responses, "https://x.test/v1"),
            "https://x.test/v1/responses"
        );
    }
}
