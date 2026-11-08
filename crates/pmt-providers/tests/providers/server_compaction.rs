//! Server-side Responses compaction on the wire (spec:
//! context-compression, Server-side Responses compaction (opt-in)): a
//! supporting route sends the compaction setting and the stored item
//! first in the input, and hands the returned item back; a route without
//! support sends neither.

use pmt_providers::StreamEvent;
use pmt_providers::request::ServerCompaction;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

use crate::support::{Env, caller, request, sse};

fn config(uri: &str) -> String {
    format!(
        "[providers.resp]\nbase_url = \"{uri}/resp/v1\"\nwire_format = \"responses\"\nkey_secret = \"RESP_KEY\"\nsupports_server_compaction = true\n\
         [providers.plain]\nbase_url = \"{uri}/plain/v1\"\nwire_format = \"responses\"\nkey_secret = \"RESP_KEY\"\n"
    )
}

const KEYS: &[(&str, &str)] = &[("RESP_KEY", "resp-key-0123456789")];

async fn sent(server: &MockServer, provider: &str) -> (Value, Vec<StreamEvent>) {
    let env = Env::new(&config(&server.uri()), KEYS);
    let route = env.route(provider, "m").await;
    let mut request = request("next question");
    request.server_compaction = Some(ServerCompaction {
        threshold: 5000,
        item: Some(json!({"type": "compaction", "id": "cmp_0", "encrypted_content": "enc_OLD"})),
    });
    let mut stream = caller().send(&route, &request, None).await.unwrap();
    let events = stream.collect().await;
    let received = server.received_requests().await.unwrap();
    let body = serde_json::from_slice(&received.last().unwrap().body).unwrap();
    (body, events)
}

#[tokio::test]
async fn a_supporting_route_sends_the_setting_and_the_item_and_returns_the_new_item() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .respond_with(sse("responses_compaction.sse"))
        .mount(&server)
        .await;
    let (body, events) = sent(&server, "resp").await;
    assert_eq!(
        body["context_management"],
        json!([{"type": "compaction", "compact_threshold": 5000}])
    );
    assert_eq!(body["input"][0]["encrypted_content"], "enc_OLD", "{body}");
    let item = events.iter().find_map(|e| match e {
        StreamEvent::CompactionItem { item } => Some(item.clone()),
        _ => None,
    });
    assert_eq!(item.unwrap()["encrypted_content"], "enc_ABC123");
}

#[tokio::test]
async fn a_route_without_support_sends_neither() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/plain/v1/responses"))
        .respond_with(sse("responses_compaction.sse"))
        .mount(&server)
        .await;
    let (body, _) = sent(&server, "plain").await;
    assert!(body.get("context_management").is_none(), "{body}");
    assert!(!body.to_string().contains("enc_OLD"), "{body}");
}
