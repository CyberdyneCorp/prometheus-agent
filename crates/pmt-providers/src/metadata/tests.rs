//! Catalog behaviour (spec: Model metadata catalog scenarios).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::Config;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as Incoming, Respond, ResponseTemplate};

use super::*;
use crate::registry::ProviderRegistry;

fn config(text: &str) -> Config {
    toml::from_str(text).unwrap()
}

fn client() -> reqwest::Client {
    pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap()
}

#[test]
fn explicit_override_wins_without_network_access() {
    let catalog = Catalog::new(
        &config("[models.\"local/qwen3\"]\ncontext_window = 131072\n"),
        None,
    );
    let (meta, sources) = catalog.metadata("local", "qwen3");
    assert_eq!(meta.context_window, 131_072);
    // `metadata` takes no client: resolving it cannot reach the network.
    assert_eq!(sources, [MetadataSource::Override]);
}

#[test]
fn unknown_model_gets_the_defaults() {
    let catalog = Catalog::new(&Config::default(), None);
    let (meta, sources) = catalog.metadata("nowhere", "mystery");
    assert_eq!(meta, ModelMetadata::default());
    assert_eq!(meta.context_window, 128_000);
    assert!(meta.supports_tools && !meta.supports_vision && meta.price.is_none());
    assert_eq!(sources, [MetadataSource::Default]);
}

#[test]
fn fields_layer_override_over_builtin_and_prices_follow() {
    let catalog = Catalog::new(
        &config("[models.\"anthropic/claude-sonnet-4-5\"]\nsupports_vision = false\n"),
        None,
    );
    let (meta, sources) = catalog.metadata("anthropic", "claude-sonnet-4-5");
    assert_eq!(meta.context_window, 200_000);
    assert!(!meta.supports_vision);
    assert_eq!(sources, [MetadataSource::Override, MetadataSource::Builtin]);
    let price = catalog.price("anthropic", "claude-sonnet-4-5").unwrap();
    assert!((price.input - 3.0).abs() < 1e-12);
    assert!(catalog.price("nowhere", "mystery").is_none());
}

struct Counting(Arc<AtomicUsize>, serde_json::Value);

impl Respond for Counting {
    fn respond(&self, _: &Incoming) -> ResponseTemplate {
        self.0.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(self.1.clone())
    }
}

#[tokio::test]
async fn provider_without_a_catalog_makes_no_listing_request() {
    let server = MockServer::start().await;
    let hits = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(Counting(hits.clone(), serde_json::json!({"data": []})))
        .mount(&server)
        .await;
    let text = format!(
        "[providers.deepseek]\nbase_url = \"{}/v1\"\nsupports_model_listing = false\n",
        server.uri()
    );
    let config = config(&text);
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let deepseek = registry.lookup("deepseek").unwrap();
    let catalog = Catalog::new(&config, None);
    assert!(
        !catalog
            .refresh(&client(), deepseek, None, Timestamp::now())
            .await
            .unwrap()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        catalog.picker_models(deepseek),
        ["deepseek-chat", "deepseek-reasoner"]
    );
}

#[tokio::test]
async fn live_listing_is_cached_for_its_ttl_and_feeds_metadata() {
    let server = MockServer::start().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let body = serde_json::json!({"data": [{"id": "qwen3", "max_model_len": 32768}]});
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(Counting(hits.clone(), body))
        .mount(&server)
        .await;
    let text = format!(
        "[providers.vllm]\nbase_url = \"{}/v1\"\ncatalog_ttl_secs = 60\n",
        server.uri()
    );
    let config = config(&text);
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let vllm = registry.lookup("vllm").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::new(&config, Some(dir.path().join("models")));
    let now = Timestamp::now();
    assert!(catalog.refresh(&client(), vllm, None, now).await.unwrap());
    assert!(!catalog.refresh(&client(), vllm, None, now).await.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(catalog.metadata("vllm", "qwen3").0.context_window, 32_768);
    assert_eq!(catalog.picker_models(vllm), ["qwen3"]);
    // A second catalog reads the disk cache; once stale it refetches.
    let again = Catalog::new(&config, Some(dir.path().join("models")));
    assert_eq!(again.metadata("vllm", "qwen3").1, [MetadataSource::Listing]);
    let later = now
        .checked_add(jiff::SignedDuration::from_secs(61))
        .unwrap();
    assert!(again.refresh(&client(), vllm, None, later).await.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
