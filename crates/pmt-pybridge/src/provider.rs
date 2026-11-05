//! Hermes model-provider plugins (spec: python-bridge, Model-provider
//! plugins through the bridge; model-providers, Plugin model providers).
//!
//! A registered `ProviderProfile` becomes a model-provider descriptor: the
//! API mode picks the wire format, `env_vars` become the secret names, the
//! base URL's host the key hosts. The host performs every HTTP request
//! and stream itself; the profile's hooks (headers, request body, model
//! listing) run in the sidecar as `provider.call`.

use serde_json::{Value, json};

/// `provider '<name>': api mode '<mode>' is not supported`, or the wire
/// format of a Hermes API mode.
///
/// # Errors
///
/// The refusal text for a mode with no Prometheus wire format.
pub fn wire_format(name: &str, mode: &str) -> Result<&'static str, String> {
    match mode {
        "" | "chat_completions" | "openai_chat" => Ok("chat_completions"),
        "codex_responses" | "responses" => Ok("responses"),
        "anthropic_messages" => Ok("anthropic_messages"),
        other => Err(format!(
            "provider '{name}': api mode '{other}' is not supported"
        )),
    }
}

/// The host of a URL (`https://api.x.com:8443/v1` -> `api.x.com`).
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The descriptor JSON of a profile the shim described.
///
/// # Errors
///
/// The refusal of an unsupported API mode.
pub fn descriptor(profile: &Value) -> Result<String, String> {
    let name = profile["name"].as_str().unwrap_or("");
    let wire = wire_format(name, profile["api_mode"].as_str().unwrap_or(""))?;
    let strings = |key: &str| -> Vec<String> {
        profile[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let base_url = profile["base_url"].as_str().filter(|u| !u.is_empty());
    let mut descriptor = json!({
        "id": name,
        "wire_format": wire,
        "aliases": strings("aliases"),
        "secret_names": strings("env_vars").into_iter().filter(|v| !v.ends_with("_BASE_URL")).collect::<Vec<_>>(),
        "key_hosts": base_url.and_then(host_of).into_iter().collect::<Vec<_>>(),
        "supports_model_listing": profile["supports_model_listing"].as_bool().unwrap_or(false)
            && hooks(profile).contains(&"fetch_models".to_owned()),
    });
    if let Some(url) = base_url {
        descriptor["base_url"] = json!(url);
    }
    if let Some(url) = profile["models_url"].as_str().filter(|u| !u.is_empty()) {
        descriptor["models_url"] = json!(url);
    }
    if let Some(model) = strings("fallback_models").first() {
        descriptor["default_model"] = json!(model);
    }
    Ok(descriptor.to_string())
}

/// The hooks a profile overrides.
#[must_use]
pub fn hooks(profile: &Value) -> Vec<String> {
    profile["hooks"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copilot_profile_becomes_a_chat_completions_descriptor() {
        let profile = json!({
            "name": "copilot", "api_mode": "chat_completions", "aliases": ["gh"],
            "env_vars": ["COPILOT_TOKEN", "COPILOT_BASE_URL"], "base_url": "https://api.githubcopilot.com",
            "supports_model_listing": true, "hooks": ["build_api_kwargs_extras"], "fallback_models": ["gpt-5"],
        });
        let descriptor: Value = serde_json::from_str(&descriptor(&profile).unwrap()).unwrap();
        assert_eq!(descriptor["wire_format"], "chat_completions");
        assert_eq!(descriptor["secret_names"], json!(["COPILOT_TOKEN"]));
        assert_eq!(descriptor["key_hosts"], json!(["api.githubcopilot.com"]));
        assert_eq!(descriptor["supports_model_listing"], false);
        pmt_plugins::surfaces::parse_descriptor(&descriptor.to_string()).unwrap();
    }

    #[test]
    fn an_api_mode_without_a_wire_format_is_refused() {
        let profile = json!({"name": "bedrock", "api_mode": "bedrock_converse"});
        assert_eq!(
            descriptor(&profile).unwrap_err(),
            "provider 'bedrock': api mode 'bedrock_converse' is not supported"
        );
    }
}
