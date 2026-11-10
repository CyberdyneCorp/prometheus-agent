//! The fast mode service tier (spec: model-providers, Fast mode service
//! tier): which requests of a turn ask for the provider's priority tier,
//! and how each provider is asked. OpenAI's own endpoints take
//! `service_tier = "priority"`; Anthropic's take fast mode (`speed =
//! "fast"` behind its beta flag). Other routes have no such tier and the
//! request is sent unchanged.

use std::time::Duration;

use pmt_config::provider_urls::host_of;
use pmt_config::schema::{FastMode, WireFormat};
use serde_json::{Value, json};

/// What one request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// The provider's priority tier.
    Fast,
    /// The provider's fastest tier, else its priority tier.
    Ultrafast,
}

/// The priority tier a route offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastTier {
    /// OpenAI priority processing (`service_tier = "priority"`).
    OpenAiPriority,
    /// Anthropic fast mode (`speed = "fast"`).
    AnthropicFast,
}

/// Anthropic's fast mode beta flag.
pub const ANTHROPIC_FAST_BETA: &str = "fast-mode-2026-02-01";

impl FastTier {
    /// The ledger's `service_tier` of a request on this tier.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenAiPriority => "priority",
            Self::AnthropicFast => "fast",
        }
    }

    /// Asks for the tier in an encoded body. Neither provider has a tier
    /// above these, so `ultrafast` asks the same.
    pub fn apply(self, body: &mut Value, _speed: Speed) {
        let Some(fields) = body.as_object_mut() else {
            return;
        };
        match self {
            Self::OpenAiPriority => fields.insert("service_tier".to_owned(), json!("priority")),
            Self::AnthropicFast => fields.insert("speed".to_owned(), json!("fast")),
        };
    }

    /// The beta flag the request needs.
    #[must_use]
    pub const fn beta(self) -> Option<&'static str> {
        match self {
            Self::OpenAiPriority => None,
            Self::AnthropicFast => Some(ANTHROPIC_FAST_BETA),
        }
    }
}

/// The tier of a route on `provider` at `base_url` over `wire`: only the
/// providers' own endpoints have one (a relay may not pass it on).
#[must_use]
pub fn tier(provider: &str, wire: WireFormat, base_url: &str) -> Option<FastTier> {
    let host = host_of(base_url)?;
    match (provider, wire, host.as_str()) {
        ("openai-api", WireFormat::Responses | WireFormat::ChatCompletions, "api.openai.com")
        | ("openai-codex", WireFormat::Responses, "chatgpt.com") => Some(FastTier::OpenAiPriority),
        ("anthropic", WireFormat::AnthropicMessages, "api.anthropic.com") => {
            Some(FastTier::AnthropicFast)
        }
        _ => None,
    }
}

/// The speed a request `elapsed` into its turn asks for under `mode`:
/// `fast`/`ultrafast` always, `auto` within `window` of every turn, `cold`
/// within `window` of the session's first turn only.
#[must_use]
pub fn wanted(
    mode: FastMode,
    elapsed: Duration,
    first_turn: bool,
    window: Duration,
) -> Option<Speed> {
    match mode {
        FastMode::Normal => None,
        FastMode::Fast => Some(Speed::Fast),
        FastMode::Ultrafast => Some(Speed::Ultrafast),
        FastMode::Auto => (elapsed < window).then_some(Speed::Fast),
        FastMode::Cold => (first_turn && elapsed < window).then_some(Speed::Fast),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_mins(1);

    #[test]
    fn auto_asks_only_within_the_window_of_each_turn() {
        let at = |s| wanted(FastMode::Auto, Duration::from_secs(s), false, WINDOW);
        assert_eq!(at(10), Some(Speed::Fast));
        assert_eq!(at(90), None);
    }

    #[test]
    fn cold_asks_only_on_the_first_turn() {
        let ten = Duration::from_secs(10);
        assert_eq!(wanted(FastMode::Cold, ten, true, WINDOW), Some(Speed::Fast));
        assert_eq!(wanted(FastMode::Cold, ten, false, WINDOW), None);
        assert_eq!(wanted(FastMode::Normal, ten, true, WINDOW), None);
        assert_eq!(
            wanted(FastMode::Ultrafast, Duration::from_mins(15), false, WINDOW),
            Some(Speed::Ultrafast)
        );
    }

    #[test]
    fn only_the_providers_own_endpoints_have_a_tier() {
        assert_eq!(
            tier(
                "openai-api",
                WireFormat::Responses,
                "https://api.openai.com/v1"
            ),
            Some(FastTier::OpenAiPriority)
        );
        assert_eq!(
            tier(
                "anthropic",
                WireFormat::AnthropicMessages,
                "https://api.anthropic.com"
            ),
            Some(FastTier::AnthropicFast)
        );
        assert_eq!(
            tier(
                "openrouter",
                WireFormat::ChatCompletions,
                "https://openrouter.ai/api/v1"
            ),
            None
        );
        assert_eq!(
            tier("openai-api", WireFormat::Responses, "https://relay.test/v1"),
            None
        );
    }
}
