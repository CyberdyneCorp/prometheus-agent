//! Local inference servers (spec: model-providers, Local server detection
//! and context probes; Context length detection chain) against recorded
//! server answers.

use std::sync::Arc;

use pmt_providers::local::{self, ServerKind, Warnings};
use pmt_providers::metadata::{Catalog, ContextSource};
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::caching::store;
use crate::support::Env;

fn client() -> reqwest::Client {
    pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap()
}

#[tokio::test]
async fn llama_cpp_context_from_props() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"default_generation_settings": {"n_ctx": 65536}, "total_slots": 1}),
        ))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!("[providers.llamacpp]\nbase_url = \"{}/v1\"\n", server.uri()),
        &[],
    );
    let route = env.route("llamacpp", "qwen3-coder").await;
    let mut warnings = Warnings::default();
    let (given, meta) =
        local::prepare_session(&client(), &env.catalog, &route, 32_000, &mut warnings).await;
    let meta = meta.unwrap();
    assert!(given.is_empty());
    assert_eq!(meta.context_window, 65_536);
    assert_eq!(meta.context_source, ContextSource::Probe);
}

#[tokio::test]
async fn root_url_normalised_to_the_v1_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "qwen3"}]})))
        .mount(&server)
        .await;
    let base = local::normalise(&client(), &server.uri()).await.unwrap();
    assert_eq!(base, format!("{}/v1", server.uri()));
    // Already a /v1 prefix: kept.
    assert_eq!(local::normalise(&client(), &base).await.unwrap(), base);
    // A server answering at its root only keeps the root.
    let root_only = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&root_only)
        .await;
    assert_eq!(
        local::normalise(&client(), &root_only.uri()).await.unwrap(),
        root_only.uri()
    );
    let nothing = MockServer::start().await;
    let err = local::normalise(&client(), &nothing.uri())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "local.server_probe");
}

#[tokio::test]
async fn small_ollama_context_is_warned_before_the_minimum_window_check() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_json(json!({"model": "qwen3"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "parameters": "num_ctx 2048",
            "model_info": {"qwen3.context_length": 40960},
            "capabilities": ["completion", "tools"]
        })))
        .mount(&server)
        .await;
    let env = Env::new(
        &format!("[providers.ollama]\nbase_url = \"{}/v1\"\n", server.uri()),
        &[],
    );
    let route = env.route("ollama", "qwen3").await;
    let mut warnings = Warnings::default();
    let (given, checked) =
        local::prepare_session(&client(), &env.catalog, &route, 64_000, &mut warnings).await;
    assert_eq!(given.len(), 1, "{given:?}");
    assert!(given[0].contains("num_ctx = 2048"), "{}", given[0]);
    let err = checked.unwrap_err();
    assert_eq!(err.code().code, "model.context_too_small");
}

#[tokio::test]
async fn native_listings_and_served_windows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"models": [{"name": "llama3:latest"}, {"name": "qwen3:8b"}]}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models": [{"type": "llm", "key": "qwen/qwen3-8b"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "served", "max_model_len": 32768}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v0/models/qwen/qwen3-8b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id": "qwen/qwen3-8b", "loaded_context_length": 8192, "max_context_length": 40960, "capabilities": ["tool_use"]}),
        ))
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri());
    let c = client();
    assert_eq!(
        local::list_models(&c, ServerKind::Ollama, &base)
            .await
            .unwrap(),
        ["llama3:latest", "qwen3:8b"]
    );
    assert_eq!(
        local::list_models(&c, ServerKind::LmStudio, &base)
            .await
            .unwrap(),
        ["qwen/qwen3-8b"]
    );
    assert_eq!(
        local::list_models(&c, ServerKind::OpenAiCompatible, &base)
            .await
            .unwrap(),
        ["served"]
    );
    let vllm = local::probe(&c, ServerKind::OpenAiCompatible, &base, "served")
        .await
        .unwrap();
    assert_eq!(vllm.context_window, Some(32_768));
    let lms = local::probe(&c, ServerKind::LmStudio, &base, "qwen/qwen3-8b")
        .await
        .unwrap();
    assert_eq!(
        (lms.context_window, lms.supports_tools),
        (Some(8_192), Some(true))
    );
}

#[tokio::test]
async fn discovered_value_reused_offline_without_warning() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"id": "vendor/model-x", "context_length": 200_000}]}),
            ),
        )
        .mount(&server)
        .await;
    let base = format!("{}/api/v1", server.uri());
    let config =
        format!("[providers.openrouter]\nbase_url = \"{base}\"\nkey_hosts = [\"127.0.0.1\"]\n");
    let env = Env::new(&config, &[("OPENROUTER_API_KEY", "sk-or-0123456789")]);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = store(&dir.path().join("home"));
    // Yesterday's session discovered the window from the listing.
    let first = Catalog::new(&env.config, None).with_store(store.clone());
    let provider = env.registry.lookup("openrouter").unwrap();
    first
        .refresh(&client(), provider, None, jiff::Timestamp::now())
        .await
        .unwrap();
    drop(server);
    // Today the listing endpoint is unreachable and nothing is cached on
    // disk: the discovered value is used, with no defaults warning.
    let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || Sink(sink.clone()))
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let second = Catalog::new(&env.config, None).with_store(store);
    let (meta, _) = second.metadata_at("openrouter", "vendor/model-x", Some(&base));
    assert_eq!(meta.context_window, 200_000);
    assert_eq!(meta.context_source, ContextSource::Discovered);
    assert!(captured.lock().unwrap().is_empty(), "no warning expected");
    // Another endpoint serving the same id does not share the value.
    let (other, _) = second.metadata_at(
        "openrouter",
        "vendor/model-x",
        Some("http://127.0.0.1:1/v1"),
    );
    assert_ne!(other.context_source, ContextSource::Discovered);
}

struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
