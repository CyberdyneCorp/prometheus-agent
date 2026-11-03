//! Sample `provider` plugin: a web-search provider with canned results,
//! and a model provider (`sample-llm`, an OpenAI-compatible endpoint).

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::provider::models;
use bindings::exports::prometheus::provider::plugin::{Category, Guest, ProviderSpec};

struct Sample;

impl Guest for Sample {
    fn providers() -> Vec<ProviderSpec> {
        vec![ProviderSpec {
            id: "sample-search".to_owned(),
            category: Category::WebSearch,
            description: "Canned search results.".to_owned(),
        }]
    }

    fn invoke(provider: String, _request: String) -> Result<String, String> {
        match provider.as_str() {
            "sample-search" => Ok(r#"{"results":[]}"#.to_owned()),
            other => Err(format!("unknown provider {other}")),
        }
    }
}

impl models::Guest for Sample {
    fn descriptors() -> Vec<String> {
        vec![
            r#"{"id":"sample-llm","wire_format":"chat_completions","base_url":"https://llm.example.com/v1","secret_names":["SAMPLE_LLM_KEY"],"key_hosts":["llm.example.com"]}"#
                .to_owned(),
        ]
    }

    fn headers(_provider: String) -> Result<Vec<(String, String)>, String> {
        Ok(vec![("x-sample".to_owned(), "1".to_owned())])
    }

    fn prepare_request(_provider: String, body: String) -> Result<String, String> {
        Ok(body)
    }

    fn list_models(_provider: String) -> Result<Vec<String>, String> {
        Ok(vec!["sample-1".to_owned()])
    }
}

bindings::export!(Sample with_types_in bindings);
