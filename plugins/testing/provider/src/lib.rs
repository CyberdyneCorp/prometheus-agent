//! Test plugin: provider objects. It registers web search and extract
//! provider `plugsearch` and image generation provider `pics`; search
//! answers one hit naming the query, extract a page naming the URL, and
//! `pics` echoes its request.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/provider"],
    world: "prometheus:provider/provider",
    generate_all,
});

use exports::prometheus::provider::plugin::{Category, Guest, ProviderSpec};

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

export!(Provider);
