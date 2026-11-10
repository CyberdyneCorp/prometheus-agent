//! Test plugin: provider objects. It registers web search and extract
//! provider `plugsearch` and image generation provider `pics`; search
//! answers one hit naming the query, extract a page naming the URL, and
//! `pics` echoes its request.
//!
//! Its `models` (provider world 1.1) register one chat-completions model
//! provider per id in the `model_ids` setting (comma list), served at the
//! `base_url` setting (default `https://llm.example.com/v1`) with secret
//! `PLUGIN_LLM_KEY`; every request to them carries header
//! `x-plugin: <id>`, and their model listing is the `listed` setting
//! (comma list; `plugin-model` when unset, `fail` fails the listing).
//!
//! With the `oauth_issuer` setting (an identity provider's base URL) each
//! model provider also declares an OAuth PKCE sign-in there (`/authorize`,
//! `/token`, client `testing-plugin`), which the host runs; its
//! `prepare-request` then copies the whole body it was given into field
//! `plugin_saw`, so a test can check the plugin got only the opaque
//! credential handle.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/messaging", "../../../wit/provider"],
    world: "prometheus:provider/provider",
    generate_all,
});

use exports::prometheus::provider::models;
use exports::prometheus::provider::plugin::{Category, Guest, ProviderSpec};
use prometheus::host::config;

struct Provider;

fn spec(id: &str, category: Category) -> ProviderSpec {
    ProviderSpec {
        id: id.to_owned(),
        category,
        description: format!("test {id}"),
    }
}

/// The value of `"<key>":"<text>"` in a flat JSON object.
fn field(json: &str, key: &str) -> String {
    json.split_once(&format!(r#""{key}":""#))
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(v, _)| v.to_owned())
        .unwrap_or_default()
}

impl Guest for Provider {
    fn providers() -> Vec<ProviderSpec> {
        vec![
            spec("plugsearch", Category::WebSearch),
            spec("plugsearch", Category::WebExtract),
            spec("pics", Category::ImageGen),
        ]
    }

    fn invoke(provider: String, request: String) -> Result<String, String> {
        match provider.as_str() {
            "plugsearch" if request.contains(r#""query""#) => Ok(format!(
                r#"{{"results":[{{"title":"hit for {q}","url":"https://example.org/{q}","snippet":"about {q}"}}]}}"#,
                q = field(&request, "query")
            )),
            "plugsearch" => Ok(format!(
                r##"{{"title":"page","markdown":"# {}"}}"##,
                field(&request, "url")
            )),
            "pics" => Ok(request),
            other => Err(format!("unknown provider {other}")),
        }
    }
}

/// A string setting (stored as a JSON string).
fn setting(key: &str) -> String {
    config::get(key)
        .ok()
        .flatten()
        .map(|v| v.trim_matches('"').to_owned())
        .unwrap_or_default()
}

impl models::Guest for Provider {
    fn descriptors() -> Vec<String> {
        let mut base = setting("base_url");
        if base.is_empty() {
            base = "https://llm.example.com/v1".to_owned();
        }
        let host = base
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', ':']).next())
            .unwrap_or_default()
            .to_owned();
        let issuer = setting("oauth_issuer");
        let oauth = if issuer.is_empty() {
            String::new()
        } else {
            format!(
                r#","oauth":{{"client_id":"testing-plugin","authorize_url":"{issuer}/authorize","token_url":"{issuer}/token","scopes":["inference"]}}"#
            )
        };
        setting("model_ids")
            .split(',')
            .filter(|id| !id.is_empty())
            .map(|id| {
                format!(
                    r#"{{"id":"{id}","wire_format":"chat_completions","base_url":"{base}","secret_names":["PLUGIN_LLM_KEY"],"key_hosts":["{host}"],"default_model":"plugin-model"{oauth}}}"#
                )
            })
            .collect()
    }

    fn headers(provider: String) -> Result<Vec<(String, String)>, String> {
        Ok(vec![("x-plugin".to_owned(), provider)])
    }

    fn prepare_request(_provider: String, body: String) -> Result<String, String> {
        if setting("oauth_issuer").is_empty() {
            return Ok(body);
        }
        let seen = body.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(body.replacen('{', &format!(r#"{{"plugin_saw":"{seen}","#), 1))
    }

    fn list_models(_provider: String) -> Result<Vec<String>, String> {
        match setting("listed").as_str() {
            "" => Ok(vec!["plugin-model".to_owned()]),
            "fail" => Err("listing failed on purpose".to_owned()),
            listed => Ok(listed.split(',').map(str::to_owned).collect()),
        }
    }
}

export!(Provider);
