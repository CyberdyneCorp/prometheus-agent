//! Empty-response and narration guards (spec: agent-turn-loop,
//! Empty-response and narration guards).
//!
//! An empty reply (no text, no tool calls) is retried at most three times
//! in a row, once when a single attempt's estimated input cost reaches
//! `agent.empty_response_guard.cost_threshold_usd`, and never when it is
//! deterministic (zero output tokens: an unsignaled refusal answers the
//! same again). A reply that announces an action but calls no tool gets at
//! most two cache-safe nudges per turn (a new user-role row) where
//! `agent.intent_continuation` applies.

use std::sync::LazyLock;

use pmt_config::schema::{AgentConfig, ContinuationMode, IntentContinuation, WireFormat};
use pmt_providers::request::Request;
use regex::Regex;

use super::model::{ModelReply, RouteInfo};

/// Empty replies retried in a row at most.
const EMPTY_RETRIES: u32 = 3;
/// Narration nudges per turn at most.
pub(super) const MAX_NUDGES: u32 = 2;
/// The nudge's text.
pub(super) const NUDGE: &str = "[prometheus] You described what you will do but did not call a tool. Continue now and call the tools.";

/// Whether an empty `reply` to `request` on `route` is retried, given the
/// `retried` empty replies before it in this streak.
pub(super) fn retry_empty(
    config: &AgentConfig,
    request: &Request,
    route: &RouteInfo,
    reply: &ModelReply,
    retried: u32,
) -> bool {
    if reply.usage.output_tokens == 0 {
        return false;
    }
    let budget = if estimated_input_cost(request, route)
        .is_some_and(|cost| cost >= config.empty_response_guard.cost_threshold_usd)
    {
        1
    } else {
        EMPTY_RETRIES
    };
    retried < budget
}

/// The input cost of `request` on `route` estimated before the provider
/// counts; `None` when the route is unpriced.
#[allow(clippy::cast_precision_loss)] // a token estimate, far below 2^52
fn estimated_input_cost(request: &Request, route: &RouteInfo) -> Option<f64> {
    let price = route.price?;
    Some(request.estimated_tokens() as f64 * price.input / 1_000_000.0)
}

/// Whether `agent.intent_continuation` applies to `route`.
pub(super) fn continuation_applies(config: &AgentConfig, route: &RouteInfo) -> bool {
    match &config.intent_continuation {
        IntentContinuation::Mode(ContinuationMode::Auto) => {
            route.wire_format == WireFormat::Responses
        }
        IntentContinuation::Enabled(on) => *on,
        IntentContinuation::Models(names) => names.iter().any(|n| route.model.contains(n.as_str())),
    }
}

/// Whether a toolless `reply` to a request that offered tools announces an
/// action it did not take ("Let me check the tests:").
pub(super) fn narrates(reply: &ModelReply, offered_tools: bool) -> bool {
    static ANNOUNCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?:ok(?:ay)?[,.]?\s+|now[,]?\s+|next[,]?\s+|first[,]?\s+)?(?:let me|let's|i'll|i will|i am going to|i'm going to)\b",
        )
        .expect("valid regex")
    });
    if !offered_tools || !reply.tool_calls.is_empty() {
        return false;
    }
    let text = reply.text.trim();
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let sentence = last
        .rsplit(['.', '!', '?'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(last)
        .trim();
    // "Let me know..." closes an answer; it announces nothing.
    !sentence.to_lowercase().starts_with("let me know") && ANNOUNCE.is_match(sentence)
}

#[cfg(test)]
mod tests {
    use pmt_obs::usage::Price;
    use pmt_providers::request::Message;

    use super::*;

    fn route(price_input: Option<f64>, wire: WireFormat, model: &str) -> RouteInfo {
        RouteInfo {
            provider: "p".into(),
            model: model.into(),
            price: price_input.map(|input| Price {
                input,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                reasoning: None,
            }),
            context_window: None,
            wire_format: wire,
        }
    }

    fn empty(output_tokens: u64) -> ModelReply {
        let mut reply = ModelReply::default();
        reply.usage.output_tokens = output_tokens;
        reply
    }

    fn request(chars: usize) -> Request {
        Request {
            messages: vec![Message::text(
                pmt_providers::request::Role::User,
                "x".repeat(chars),
            )],
            ..Request::default()
        }
    }

    #[test]
    fn an_expensive_empty_is_retried_once() {
        let config = AgentConfig::default();
        // 400k tokens at $1/M = $0.40 >= $0.25.
        let (req, r) = (
            request(1_600_000),
            route(Some(1.0), WireFormat::ChatCompletions, "m"),
        );
        assert!(retry_empty(&config, &req, &r, &empty(5), 0));
        assert!(!retry_empty(&config, &req, &r, &empty(5), 1));
        let cheap = request(400);
        assert!(retry_empty(&config, &cheap, &r, &empty(5), 2));
        assert!(!retry_empty(&config, &cheap, &r, &empty(5), 3));
    }

    #[test]
    fn a_deterministic_empty_is_not_retried() {
        let r = route(None, WireFormat::ChatCompletions, "m");
        assert!(!retry_empty(
            &AgentConfig::default(),
            &request(10),
            &r,
            &empty(0),
            0
        ));
    }

    #[test]
    fn continuation_follows_the_setting() {
        let mut config = AgentConfig::default();
        assert!(continuation_applies(
            &config,
            &route(None, WireFormat::Responses, "gpt")
        ));
        assert!(!continuation_applies(
            &config,
            &route(None, WireFormat::ChatCompletions, "gpt")
        ));
        config.intent_continuation = IntentContinuation::Models(vec!["qwen".into()]);
        assert!(continuation_applies(
            &config,
            &route(None, WireFormat::ChatCompletions, "qwen3")
        ));
        config.intent_continuation = IntentContinuation::Enabled(false);
        assert!(!continuation_applies(
            &config,
            &route(None, WireFormat::Responses, "gpt")
        ));
    }

    #[test]
    fn announcements_without_calls_are_narration() {
        let reply = |text: &str| ModelReply {
            text: text.into(),
            ..ModelReply::default()
        };
        assert!(narrates(
            &reply("I found the bug. Let me fix it now."),
            true
        ));
        assert!(narrates(&reply("Now I'll run the tests:"), true));
        assert!(!narrates(&reply("The tests pass. Done."), true));
        assert!(!narrates(&reply("Let me know if you need more."), true));
        assert!(
            !narrates(&reply("Let me fix it."), false),
            "no tools offered"
        );
    }
}
