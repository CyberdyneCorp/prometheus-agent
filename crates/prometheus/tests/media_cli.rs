//! The web and media categories through the binary (spec:
//! web-and-media-tools, Media tool credentials and billing are explicit;
//! Built-in web backends with capability flags; Keyless web search rotation
//! is explicit opt-in): `tools explain` and `doctor providers` show the
//! resolved provider, billing source and availability.

mod common;

use common::{append_config, mock_home, ok, run, run_with_stdin, temp_home};
use serde_json::Value;

fn explain(home: &std::path::Path, tool: &str) -> Value {
    serde_json::from_str(&ok(&run(home, &["--json", "tools", "explain", tool]))).unwrap()
}

#[test]
fn tools_explain_names_the_provider_billing_and_availability() {
    let home = temp_home();
    let image = explain(home.path(), "image_generate");
    assert_eq!(image["media"]["category"], "image_generation");
    assert_eq!(
        image["media"]["availability"],
        "unavailable:FAL_KEY is not set"
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "FAL_KEY"],
        "fal-test\n",
    ));
    let image = explain(home.path(), "image_generate");
    assert_eq!(image["media"]["provider"], "fal (fal-ai/flux-2/klein/9b)");
    assert_eq!(image["media"]["billing_source"], "api_key:fal");
    assert_eq!(image["media"]["availability"], "available");
    let speech = explain(home.path(), "text_to_speech");
    assert_eq!(speech["media"]["provider"], "edge");
    assert_eq!(speech["media"]["billing_source"], "keyless:edge");
    let search = explain(home.path(), "web_search");
    assert_eq!(
        search["media"]["availability"],
        "unavailable:no web backend configured"
    );
    let text = ok(&run(home.path(), &["tools", "explain", "image_generate"]));
    assert!(text.contains("billing_source = api_key:fal"), "{text}");
}

#[test]
fn the_secret_store_makes_auto_web_search_available_in_explain() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "TAVILY_API_KEY"],
        "tv-test\n",
    ));
    let search = explain(home.path(), "web_search");
    assert_eq!(search["backend"], "tavily");
    assert_eq!(search["media"]["billing_source"], "api_key:tavily");
    // Sessions see the same presence: the probe passes.
    assert_eq!(search["exposure"], "exposed");
}

#[test]
fn doctor_lists_media_categories_and_keyless_traffic() {
    let home = mock_home();
    append_config(
        home.path(),
        "[web]\nbackend = \"keyless\"\n\n[web.provider_tier]\nexa = \"paid\"\n\n[tts]\nprovider = \"elevenlabs\"\n",
    );
    let out = run(home.path(), &["doctor", "providers"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("tts: unavailable:ELEVENLABS_API_KEY is not set"),
        "{text}"
    );
    assert!(
        text.contains("web_search: keyless (keyless:<ring vendor>)"),
        "{text}"
    );
    assert!(
        text.contains(
            "keyless web search sends queries and URLs to exa, parallel, firecrawl, keenable"
        ),
        "{text}"
    );
    assert!(
        text.contains("web.provider_tier.exa = \"paid\" without its key"),
        "{text}"
    );
}
