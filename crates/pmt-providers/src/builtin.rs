//! Built-in provider descriptors (spec: model-providers, Built-in provider
//! catalog). This is the subset the M1 resolution and wire adapters are
//! verified against; the rest of the spec table is added by the catalog
//! task with its own table tests.

use pmt_config::schema::{ProviderAuth, WireFormat};

use crate::descriptor::Descriptor;

struct Row {
    id: &'static str,
    aliases: &'static [&'static str],
    secrets: &'static [&'static str],
    base_url: Option<&'static str>,
    wire: WireFormat,
    auth: ProviderAuth,
    default_model: Option<&'static str>,
}

const fn row(
    id: &'static str,
    aliases: &'static [&'static str],
    secrets: &'static [&'static str],
    base_url: Option<&'static str>,
    wire: WireFormat,
    auth: ProviderAuth,
) -> Row {
    Row {
        id,
        aliases,
        secrets,
        base_url,
        wire,
        auth,
        default_model: None,
    }
}

const fn with_model(mut row: Row, model: &'static str) -> Row {
    row.default_model = Some(model);
    row
}

use ProviderAuth::{ApiKey, None as Keyless};
use WireFormat::{AnthropicMessages, ChatCompletions, Responses};

const ROWS: &[Row] = &[
    with_model(
        row(
            "openrouter",
            &["or"],
            &["OPENROUTER_API_KEY"],
            Some("https://openrouter.ai/api/v1"),
            ChatCompletions,
            ApiKey,
        ),
        "anthropic/claude-sonnet-4.5",
    ),
    with_model(
        row(
            "anthropic",
            &["claude", "claude-oauth", "claude-code"],
            &["ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN"],
            Some("https://api.anthropic.com"),
            AnthropicMessages,
            ApiKey,
        ),
        "claude-sonnet-4-5",
    ),
    with_model(
        row(
            "openai-api",
            &["openai"],
            &["OPENAI_API_KEY"],
            Some("https://api.openai.com/v1"),
            Responses,
            ApiKey,
        ),
        "gpt-5",
    ),
    with_model(
        row(
            "nous",
            &["nous-portal", "nousresearch"],
            &["NOUS_API_KEY"],
            Some("https://inference-api.nousresearch.com/v1"),
            ChatCompletions,
            ApiKey,
        ),
        "Hermes-4-405B",
    ),
    with_model(
        row(
            "xai",
            &["grok", "x-ai", "x.ai"],
            &["XAI_API_KEY"],
            Some("https://api.x.ai/v1"),
            Responses,
            ApiKey,
        ),
        "grok-4",
    ),
    with_model(
        row(
            "deepseek",
            &["deepseek-chat", "deep-seek"],
            &["DEEPSEEK_API_KEY"],
            Some("https://api.deepseek.com/v1"),
            ChatCompletions,
            ApiKey,
        ),
        "deepseek-chat",
    ),
    with_model(
        row(
            "minimax",
            &["mini-max"],
            &["MINIMAX_API_KEY"],
            Some("https://api.minimax.io/anthropic"),
            AnthropicMessages,
            ApiKey,
        ),
        "MiniMax-M2",
    ),
    row(
        "lmstudio",
        &["lm-studio", "lm_studio"],
        &["LM_API_KEY"],
        Some("http://127.0.0.1:1234/v1"),
        ChatCompletions,
        Keyless,
    ),
    row(
        "ollama",
        &[],
        &[],
        Some("http://127.0.0.1:11434/v1"),
        ChatCompletions,
        Keyless,
    ),
    row("vllm", &[], &[], None, ChatCompletions, Keyless),
    row("sglang", &[], &[], None, ChatCompletions, Keyless),
    row("custom", &["local"], &[], None, ChatCompletions, ApiKey),
];

/// Ids of local inference servers (default endpoint probed, never routed
/// to a remote provider).
const LOCAL: &[&str] = &["lmstudio", "ollama", "vllm", "sglang"];

/// Every secret name a built-in provider reads, in table order: the
/// provider-native environment variables the launch environment treats as
/// secrets.
#[must_use]
pub fn secret_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = ROWS
        .iter()
        .flat_map(|r| r.secrets.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Every built-in descriptor.
#[must_use]
pub fn descriptors() -> Vec<Descriptor> {
    ROWS.iter().map(descriptor).collect()
}

fn descriptor(row: &Row) -> Descriptor {
    let mut d = Descriptor::new(row.id, row.wire, row.base_url);
    d.aliases = row.aliases.iter().map(|s| (*s).to_owned()).collect();
    d.secret_names = row.secrets.iter().map(|s| (*s).to_owned()).collect();
    d.auth = row.auth;
    d.local = LOCAL.contains(&row.id);
    d.default_model = row.default_model.map(str::to_owned);
    d.supports_prompt_cache = matches!(row.id, "xai" | "openai-api" | "openrouter");
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_satisfies_the_descriptor_contract() {
        for d in descriptors() {
            d.validate().unwrap();
            if let Some(url) = &d.base_url {
                // key_hosts equal the default base URL's host.
                assert_eq!(
                    d.key_hosts,
                    vec![pmt_config::provider_urls::host_of(url).unwrap()],
                    "{}",
                    d.id
                );
            }
        }
    }
}
