//! The reasoning effort a session's requests carry (spec: model-providers,
//! Reasoning effort defaults, overrides and display): the config's level
//! by default, the session's `/reasoning` level from the next request,
//! clamped to what the route's model supports.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::ModelPort as _;
use pmt_agent::turn::model::{NoDeltas, ProviderModel, ProviderSetup};
use pmt_config::schema::ReasoningLevel;
use pmt_core::secret::SecretString;
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::metadata::Catalog;
use pmt_providers::request::{Message, Request, Role};
use pmt_providers::resolve::{NoExtras, SecretSource, Target, TcpProbe};
use pmt_providers::{ModelCaller, ProviderRegistry};
use pmt_store::Source;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::support::Profile;

const ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

async fn model_on(profile: &Profile) -> ProviderModel {
    let config = Arc::new(profile.scope.config().clone());
    let registry = Arc::new(ProviderRegistry::for_profile(&config, Vec::new()).unwrap());
    let secrets: Arc<dyn SecretSource> = Arc::new(BTreeMap::<String, SecretString>::new());
    let setup = ProviderSetup {
        catalog: Arc::new(Catalog::new(&config, None)),
        config,
        registry,
        secrets,
        extras: Arc::new(NoExtras),
        probe: Arc::new(TcpProbe {
            timeout: Duration::from_millis(200),
        }),
        health: Arc::new(Healthy),
        ad_hoc: None,
    };
    let caller = ModelCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap());
    let target = Target {
        provider: "acme".into(),
        model: Some("r1".into()),
        ..Target::default()
    };
    ProviderModel::start(setup, caller, &target).await.unwrap()
}

/// The `reasoning_effort` of every request the server got.
async fn sent(server: &MockServer) -> Vec<Option<String>> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["reasoning_effort"].as_str().map(str::to_owned)
        })
        .collect()
}

#[tokio::test]
async fn the_session_level_applies_from_the_next_request_clamped_to_the_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ANSWER),
        )
        .mount(&server)
        .await;
    let profile = Profile::new(&format!(
        "[providers.acme]\nbase_url = \"{}/v1\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\
         [models.\"acme/r1\"]\nsupports_reasoning = true\nmax_reasoning_effort = \"high\"\n\
         [agent]\nreasoning_effort = \"low\"\n",
        server.uri()
    ));
    let session = profile.session(Source::Cli);
    let model = model_on(&profile).await;
    let request = Request {
        messages: vec![Message::text(Role::User, "hi")],
        ..Request::default()
    };
    let ctx = CallContext::new(session, "main");
    model.call(&request, &ctx, &NoDeltas).await.unwrap();
    model.set_reasoning(Some(ReasoningLevel::Max));
    model.call(&request, &ctx, &NoDeltas).await.unwrap();
    assert_eq!(
        sent(&server).await,
        [Some("low".to_owned()), Some("high".to_owned())],
        "config level first, then the session's /reasoning level clamped to the model's maximum"
    );
}
