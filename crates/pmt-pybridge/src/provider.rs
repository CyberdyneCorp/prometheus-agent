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
    if let Some(refusal) = profile["oauth_refused"].as_str() {
        return Err(refusal.to_owned());
    }
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
    // The declared PKCE sign-in, run by the host (the sidecar keeps no
    // verifier, `state` or token).
    if profile["oauth"].is_object() {
        descriptor["oauth"] = profile["oauth"].clone();
    }
    // An agent CLI the user then names in `providers.<id>.command`.
    if let Some(command) = profile["process_command"]
        .as_str()
        .filter(|c| !c.is_empty())
    {
        descriptor["command"] = json!(command);
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
    fn a_declared_pkce_sign_in_reaches_the_descriptor_and_a_broker_is_refused() {
        let oauth = json!({"client_id": "pub", "authorize_url": "https://auth.example.com/authorize",
            "token_url": "https://auth.example.com/token", "scopes": ["inference"], "allowed_hosts": [],
            "redirect_port": 0, "redirect_path": "/callback", "authorize_params": {}});
        let profile = json!({"name": "example-pkce", "api_mode": "chat_completions",
            "base_url": "https://api.example.com/v1", "oauth": oauth});
        let text = descriptor(&profile).unwrap();
        let parsed = pmt_plugins::surfaces::parse_descriptor(&text).unwrap();
        assert_eq!(parsed.auth, pmt_config::schema::ProviderAuth::Oauth);
        assert_eq!(parsed.oauth.unwrap().client_id, "pub");
        let brokered = json!({"name": "solstice", "oauth_refused": "provider 'solstice': a token_request broker would receive the PKCE verifier"});
        assert!(descriptor(&brokered).unwrap_err().contains("PKCE verifier"));
    }

    #[test]
    fn an_external_process_profile_only_suggests_its_cli() {
        let profile = json!({"name": "kimchi", "api_mode": "chat_completions", "auth_type": "external_process",
            "process_command": "kimchi"});
        let parsed =
            pmt_plugins::surfaces::parse_descriptor(&descriptor(&profile).unwrap()).unwrap();
        assert_eq!(
            parsed.auth,
            pmt_config::schema::ProviderAuth::ExternalProcess
        );
        assert_eq!(parsed.command.as_deref(), Some("kimchi"));
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
