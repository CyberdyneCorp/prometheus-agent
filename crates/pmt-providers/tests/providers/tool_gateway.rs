//! Subscription tool gateway routing (spec: model-providers, Subscription
//! tool gateway routing) against a mock gateway.

use std::collections::BTreeMap;

use pmt_config::Config;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_providers::auth::gateway::{self, Category, Routing, ToolRouting};
use pmt_providers::resolve::{CredentialExtras, NoExtras, SignedIn};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A Nous Portal sign-in labelled `personal`.
struct SignedInNous;

impl CredentialExtras for SignedInNous {
    fn subscription(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
        Ok((provider == "nous").then(|| SignedIn {
            label: "personal".into(),
            token: SecretString::new("nous-sub-token"),
            account_id: None,
            ineligible: false,
            borrowed: None,
        }))
    }
}

fn keys(pairs: &[(&str, &str)]) -> BTreeMap<String, SecretString> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
        .collect()
}

fn config(text: &str) -> Config {
    toml::from_str(text).unwrap()
}

/// Scenario: Stored selection wins over a present key.
#[tokio::test]
async fn a_stored_nous_selection_goes_through_the_gateway_despite_a_vendor_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images"))
        .and(header("authorization", "Bearer nous-sub-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"url": "https://img"})))
        .mount(&server)
        .await;
    let config = config(&format!(
        "[image_gen]\nprovider = \"nous\"\n[tool_gateway.urls]\nimage = \"{}\"\n",
        server.uri()
    ));
    let secrets = keys(&[("FAL_KEY", "fal-key-0123456789")]);
    let routing = Routing {
        config: &config,
        secrets: &secrets,
        extras: &SignedInNous,
    };
    let route = routing.route(Category::ImageGeneration).unwrap();
    let ToolRouting::Gateway { billing_source, .. } = &route else {
        panic!("{route:?}");
    };
    assert_eq!(billing_source, "subscription:nous/personal");
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    let answer = gateway::post(&client, &route, "/v1/images", &json!({"prompt": "a cat"}))
        .await
        .unwrap();
    assert_eq!(answer["url"], "https://img");
}

/// Scenario: Selected vendor without key.
#[test]
fn a_selected_vendor_without_its_key_fails_naming_the_selection() {
    let config = config("[image_gen]\nprovider = \"fal\"\n");
    let secrets = keys(&[]);
    let routing = Routing {
        config: &config,
        secrets: &secrets,
        extras: &SignedInNous,
    };
    let err = routing.route(Category::ImageGeneration).unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    assert!(
        err.message().contains("image_gen.provider"),
        "{}",
        err.message()
    );
}

#[test]
fn only_a_never_configured_category_is_detected() {
    let secrets = keys(&[("ELEVENLABS_API_KEY", "el-key-0123456789")]);
    let unset = Config::default();
    let detect = |extras: &dyn CredentialExtras, category| {
        Routing {
            config: &unset,
            secrets: &secrets,
            extras,
        }
        .route(category)
        .unwrap()
    };
    assert!(matches!(
        detect(&SignedInNous, Category::Tts),
        ToolRouting::Vendor { ref vendor, .. } if vendor == "elevenlabs"
    ));
    assert!(matches!(
        detect(&SignedInNous, Category::Stt),
        ToolRouting::Gateway { .. }
    ));
    assert!(matches!(
        detect(&NoExtras, Category::Stt),
        ToolRouting::Unset
    ));
    // A configured browser stays where it is configured.
    assert!(matches!(
        detect(&SignedInNous, Category::Browser),
        ToolRouting::Vendor { ref vendor, .. } if vendor == "local"
    ));
}

#[tokio::test]
async fn a_gateway_call_after_the_subscription_expired_names_the_portal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(402))
        .mount(&server)
        .await;
    let config = config(&format!(
        "[tts]\nprovider = \"nous\"\n[tool_gateway.urls]\ntts = \"{}\"\n",
        server.uri()
    ));
    let secrets = keys(&[]);
    let route = Routing {
        config: &config,
        secrets: &secrets,
        extras: &SignedInNous,
    }
    .route(Category::Tts)
    .unwrap();
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    let err = gateway::post(&client, &route, "speak", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "auth.plan_not_eligible");
    assert!(
        err.hint()
            .is_some_and(|h| h.contains("portal.nousresearch.com")),
        "{:?}",
        err.hint()
    );
}
