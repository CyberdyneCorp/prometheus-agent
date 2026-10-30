//! Model metadata shipped with the binary (the third source of the
//! catalog, after config overrides and live listings). Prices are USD per
//! million tokens: input, output, cache read, cache write.

use super::ModelMetadata;
use pmt_obs::usage::Price;

struct Entry {
    provider: &'static str,
    model: &'static str,
    context_window: u64,
    max_output_tokens: u64,
    vision: bool,
    reasoning: bool,
    price: [f64; 4],
}

const fn entry(
    provider: &'static str,
    model: &'static str,
    context_window: u64,
    max_output_tokens: u64,
    vision: bool,
    reasoning: bool,
    price: [f64; 4],
) -> Entry {
    Entry {
        provider,
        model,
        context_window,
        max_output_tokens,
        vision,
        reasoning,
        price,
    }
}

const ENTRIES: &[Entry] = &[
    entry(
        "anthropic",
        "claude-opus-4-1",
        200_000,
        32_000,
        true,
        true,
        [15.0, 75.0, 1.5, 18.75],
    ),
    entry(
        "anthropic",
        "claude-sonnet-4-5",
        200_000,
        64_000,
        true,
        true,
        [3.0, 15.0, 0.3, 3.75],
    ),
    entry(
        "anthropic",
        "claude-haiku-4-5",
        200_000,
        64_000,
        true,
        true,
        [1.0, 5.0, 0.1, 1.25],
    ),
    entry(
        "openai-api",
        "gpt-5",
        400_000,
        128_000,
        true,
        true,
        [1.25, 10.0, 0.125, 0.0],
    ),
    entry(
        "openai-api",
        "gpt-5-mini",
        400_000,
        128_000,
        true,
        true,
        [0.25, 2.0, 0.025, 0.0],
    ),
    entry(
        "openai-api",
        "gpt-4.1",
        1_047_576,
        32_768,
        true,
        false,
        [2.0, 8.0, 0.5, 0.0],
    ),
    entry(
        "xai",
        "grok-4",
        256_000,
        64_000,
        true,
        true,
        [3.0, 15.0, 0.75, 0.0],
    ),
    entry(
        "deepseek",
        "deepseek-chat",
        128_000,
        8_192,
        false,
        false,
        [0.28, 0.42, 0.028, 0.0],
    ),
    entry(
        "deepseek",
        "deepseek-reasoner",
        128_000,
        64_000,
        false,
        true,
        [0.28, 0.42, 0.028, 0.0],
    ),
    entry(
        "minimax",
        "MiniMax-M2",
        204_800,
        131_072,
        false,
        true,
        [0.3, 1.2, 0.03, 0.375],
    ),
    entry(
        "openrouter",
        "anthropic/claude-sonnet-4.5",
        1_000_000,
        64_000,
        true,
        true,
        [3.0, 15.0, 0.3, 3.75],
    ),
];

/// The shipped metadata of `provider`/`model`.
pub(super) fn lookup(provider: &str, model: &str) -> Option<ModelMetadata> {
    ENTRIES
        .iter()
        .find(|e| e.provider == provider && e.model == model)
        .map(metadata)
}

/// Model ids shipped for `provider`.
pub(super) fn models(provider: &str) -> Vec<String> {
    ENTRIES
        .iter()
        .filter(|e| e.provider == provider)
        .map(|e| e.model.to_owned())
        .collect()
}

fn metadata(e: &Entry) -> ModelMetadata {
    let [input, output, cache_read, cache_write] = e.price;
    ModelMetadata {
        context_window: e.context_window,
        max_output_tokens: Some(e.max_output_tokens),
        supports_tools: true,
        supports_vision: e.vision,
        supports_video: false,
        supports_reasoning: e.reasoning,
        price: Some(Price {
            input,
            output,
            cache_read,
            cache_write,
            reasoning: None,
        }),
    }
}
