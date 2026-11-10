//! OpenRouter's live image catalog against a mock API (spec:
//! web-and-media-tools, Built-in image generation providers and models:
//! `openrouter` (live image catalog); `image_generate` tool: editing
//! advertised only when supported). Its own test binary: the catalog is
//! cached process-wide.

mod support;

use std::sync::Arc;

use pmt_config::Config;
use pmt_tools::result::Status;
use pmt_tools::variants::RouteTraits;
use serde_json::{Value, json};
use support::{Rig, details, png};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An OpenRouter API listing an editing model, a text-only image model
/// and a chat model, and answering image chats with `/out.png`.
async fn openrouter() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("output_modalities", "image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "acme/painter-2", "architecture": {
                "input_modalities": ["text", "image"], "output_modalities": ["image", "text"]}},
            {"id": "acme/sketch", "architecture": {
                "input_modalities": ["text"], "output_modalities": ["image"]}},
            {"id": "acme/chat", "architecture": {
                "input_modalities": ["text", "image"], "output_modalities": ["text"]}}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/out.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(png(), "image/png"))
        .mount(&server)
        .await;
    let image = format!("{}/out.png", server.uri());
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"choices": [{"message": {
            "images": [{"image_url": {"url": image}}]}}]})),
        )
        .mount(&server)
        .await;
    server
}

fn configured(model: &str) -> Config {
    let mut config = Config::default();
    config.image_gen.provider = Some("openrouter".into());
    config.image_gen.model = Some(model.into());
    config
}

fn advertised(config: &Config) -> (String, Value) {
    let sessions = Arc::new(pmt_media::MediaSessions::default());
    let tool = pmt_media::image::tool::registered(&sessions);
    let shown = tool.advertised(config, &RouteTraits::default());
    (shown.description, shown.parameters)
}

#[tokio::test]
async fn editing_follows_the_live_catalogs_input_modalities() {
    let server = openrouter().await;
    let painter = configured("acme/painter-2");
    let rig = Rig::new(painter.clone())
        .secret("OPENROUTER_API_KEY")
        .at("openrouter", &server.uri());
    let session = rig.session();
    session.prime().await;

    let (description, schema) = advertised(&painter);
    assert!(
        description.contains("Editing is available with acme/painter-2"),
        "{description}"
    );
    assert!(schema["properties"].get("image_url").is_some());
    assert_eq!(
        schema["properties"]["reference_image_urls"]["maxItems"],
        pmt_media::image::openrouter::REFERENCE_LIMIT
    );
    let result = session
        .generate_image(&json!({"prompt": "repaint", "image_url": "https://a.test/src.png"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(details(&result.text_content())["model"], "acme/painter-2");
    let sent = &server.received_requests().await.unwrap();
    let chat = sent
        .iter()
        .find(|r| r.url.path() == "/chat/completions")
        .unwrap();
    let body: Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(
        body["messages"][0]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "https://a.test/src.png"}})
    );

    // A model the catalog lists without image input only generates.
    let sketch = configured("acme/sketch");
    let (description, schema) = advertised(&sketch);
    assert!(description.contains("cannot edit"), "{description}");
    assert!(schema["properties"].get("image_url").is_none());
    let refused = Rig::new(sketch)
        .secret("OPENROUTER_API_KEY")
        .at("openrouter", &server.uri())
        .session()
        .generate_image(&json!({"prompt": "x", "image_url": "https://a.test/src.png"}))
        .await;
    assert_eq!(refused.status, Status::Error);
    assert!(
        refused
            .text_content()
            .contains("acme/sketch cannot take image inputs"),
        "{}",
        refused.text_content()
    );
    // One fetch served the surface and both calls (the cache is fresh).
}

#[tokio::test]
async fn a_call_fetches_the_catalog_when_the_session_did_not() {
    // Unknown to the built-in list: without the catalog it could not
    // take references. The mock expects exactly one listing.
    let server = openrouter().await;
    let result = Rig::new(configured("acme/painter-2"))
        .secret("OPENROUTER_API_KEY")
        .at("openrouter", &server.uri())
        .session()
        .generate_image(&json!({"prompt": "x", "reference_image_urls": ["https://a.test/r.png"]}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
}
