//! Sample `provider` plugin: a web-search provider with canned results.

#[allow(warnings)]
mod bindings;

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

bindings::export!(Sample with_types_in bindings);
