//! Checks of the children's direct endpoint and request overrides (spec:
//! subagent-delegation, Child request overrides and direct endpoints):
//! `delegation.api_key_secret` and `delegation.wire_format` belong to a
//! `delegation.base_url`, which must be a usable `http(s)` URL, and every
//! top-level key of `delegation.request_overrides` must be a request
//! parameter of the children's wire format (`extra_body` is free-form but
//! must be a table).

use pmt_core::error::{Error, codes};

use crate::provider_urls::{check_url, problem_error};
use crate::schema::{DelegationConfig, WireFormat};

/// The override key merged one level into the request body.
pub const EXTRA_BODY: &str = "extra_body";

/// Request parameters a Chat Completions request takes.
const CHAT_COMPLETIONS: &[&str] = &[
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "stop",
    "n",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "user",
    "response_format",
    "parallel_tool_calls",
    "tool_choice",
    "reasoning_effort",
    "service_tier",
    "store",
    "metadata",
    "prediction",
    "verbosity",
    "modalities",
    "web_search_options",
    "stream_options",
];

/// Request parameters a Responses request takes.
const RESPONSES: &[&str] = &[
    "temperature",
    "top_p",
    "max_output_tokens",
    "max_tool_calls",
    "reasoning",
    "text",
    "truncation",
    "user",
    "store",
    "metadata",
    "parallel_tool_calls",
    "tool_choice",
    "service_tier",
    "include",
    "prompt_cache_key",
    "safety_identifier",
    "top_logprobs",
];

/// Request parameters an Anthropic Messages request takes.
const ANTHROPIC_MESSAGES: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "stop_sequences",
    "metadata",
    "thinking",
    "tool_choice",
    "service_tier",
    "container",
    "mcp_servers",
];

/// The parameters `wire` takes.
#[must_use]
pub fn parameters(wire: WireFormat) -> &'static [&'static str] {
    match wire {
        WireFormat::ChatCompletions => CHAT_COMPLETIONS,
        WireFormat::Responses => RESPONSES,
        WireFormat::AnthropicMessages => ANTHROPIC_MESSAGES,
    }
}

/// The wire format of a direct endpoint at `url` with no explicit one:
/// the custom provider's rule (an `/anthropic` or `/anthropic/v1` path is
/// Anthropic Messages, anything else Chat Completions).
#[must_use]
pub fn inferred_wire(url: &str) -> WireFormat {
    let path = url.trim_end_matches('/');
    if path.ends_with("/anthropic") || path.ends_with("/anthropic/v1") {
        WireFormat::AnthropicMessages
    } else {
        WireFormat::ChatCompletions
    }
}

/// The children's wire format when the configuration fixes it: the
/// explicit one, else the direct endpoint's inferred one. `None` when
/// children inherit a route, whose format is only known at call time.
#[must_use]
pub fn children_wire(delegation: &DelegationConfig) -> Option<WireFormat> {
    delegation
        .wire_format
        .or_else(|| delegation.base_url.as_deref().map(inferred_wire))
}

/// Checks the direct endpoint and the overrides.
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the key.
pub fn check(delegation: &DelegationConfig) -> Result<(), Error> {
    check_endpoint(delegation)?;
    check_overrides(delegation)
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

fn check_endpoint(delegation: &DelegationConfig) -> Result<(), Error> {
    let Some(url) = delegation.base_url.as_deref() else {
        for (set, key) in [
            (delegation.api_key_secret.is_some(), "api_key_secret"),
            (delegation.wire_format.is_some(), "wire_format"),
        ] {
            if set {
                return Err(invalid(format!(
                    "delegation.{key} applies to a direct endpoint: set delegation.base_url too"
                )));
            }
        }
        return Ok(());
    };
    check_url(url, false)
        .map_err(|problem| problem_error("delegation.base_url", "custom", url, &problem))
}

fn check_overrides(delegation: &DelegationConfig) -> Result<(), Error> {
    let wire = children_wire(delegation);
    for (key, value) in &delegation.request_overrides {
        if key == EXTRA_BODY {
            if !value.is_object() {
                return Err(invalid(
                    "delegation.request_overrides.extra_body must be a table".to_owned(),
                ));
            }
            continue;
        }
        let supported = match wire {
            Some(wire) => parameters(wire).contains(&key.as_str()),
            None => [
                WireFormat::ChatCompletions,
                WireFormat::Responses,
                WireFormat::AnthropicMessages,
            ]
            .into_iter()
            .any(|w| parameters(w).contains(&key.as_str())),
        };
        if !supported {
            let format = wire.map_or("any supported wire format", WireFormat::as_str);
            return Err(invalid(format!(
                "delegation.request_overrides.{key}: '{key}' is not a request parameter of \
                 {format} (put provider-specific fields under \
                 delegation.request_overrides.extra_body)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegation(toml_text: &str) -> DelegationConfig {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn an_override_the_wire_format_lacks_fails() {
        let config = delegation(
            "base_url = \"https://api.example.com/anthropic\"\n\
             [request_overrides]\nreasoning_effort = \"high\"\n",
        );
        let err = check(&config).unwrap_err();
        assert_eq!(err.kind().exit_code(), 78);
        assert!(
            err.message()
                .contains("'reasoning_effort' is not a request parameter of anthropic_messages"),
            "{}",
            err.message()
        );
        let config = delegation(
            "base_url = \"https://api.example.com/v1\"\n\
             [request_overrides]\nreasoning_effort = \"high\"\n\
             [request_overrides.extra_body.provider]\nsort = \"throughput\"\n",
        );
        check(&config).unwrap();
    }

    #[test]
    fn an_inherited_route_accepts_any_known_parameter_and_extra_body_is_a_table() {
        check(&delegation("[request_overrides]\ntop_k = 5\n")).unwrap();
        assert!(check(&delegation("[request_overrides]\nbogus = 1\n")).is_err());
        assert!(check(&delegation("[request_overrides]\nextra_body = 3\n")).is_err());
    }

    #[test]
    fn endpoint_settings_need_a_usable_base_url() {
        let err = check(&delegation("wire_format = \"responses\"\n")).unwrap_err();
        assert!(err.message().contains("set delegation.base_url"));
        assert!(check(&delegation("base_url = \"http://models.example.com/v1\"\n")).is_err());
        check(&delegation(
            "base_url = \"http://127.0.0.1:8080/v1\"\napi_key_secret = \"CHILD_KEY\"\n",
        ))
        .unwrap();
    }
}
