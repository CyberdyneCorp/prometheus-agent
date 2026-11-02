//! The webhook adapter (spec: platform-adapters, Webhook routes; Shared
//! HTTP listener for webhook-style platforms): the conformance kit over a
//! real listener, unsigned and wrongly signed requests, event filters,
//! prompt rendering, redeliveries.

use std::sync::{Arc, Mutex};

use pmt_adapters_webhook::mock::{self, SECRET, SECRET_NAME, TABLE};
use pmt_adapters_webhook::{Webhook, github_signature};
use pmt_gateway::messaging::conformance::{self, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, HttpRequest,
    Inbound, SenderVerdict,
};

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        let mut events = self.events.lock().unwrap();
        let duplicate = events.iter().any(|e| e.message_id == inbound.message_id);
        if !duplicate {
            events.push(inbound);
        }
        Box::pin(async move {
            if duplicate {
                Admission::Duplicate
            } else {
                Admission::Accepted
            }
        })
    }

    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
}

fn post(route: &str, headers: &[(&str, &str)], body: &str) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        path: format!("/{route}"),
        query: None,
        headers: headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

fn signed(event: &str, delivery: &str, body: &str) -> HttpRequest {
    let sig = github_signature(SECRET, body.as_bytes());
    post(
        "pr",
        &[
            ("x-github-event", event),
            ("x-github-delivery", delivery),
            ("x-hub-signature-256", &sig),
        ],
        body,
    )
}

async fn connected(table: &str) -> (tempfile::TempDir, Webhook, Arc<Recorder>) {
    let home = tempfile::tempdir().unwrap();
    let setup = kit_setup(home.path(), "webhook", table, &[(SECRET_NAME, SECRET)]).unwrap();
    let webhook = Webhook::new(&setup);
    let link = Arc::new(Recorder::default());
    webhook.connect(Arc::clone(&link) as _).await.unwrap();
    (home, webhook, link)
}

const PR_ROUTE: &str = "routes.pr = { secret_secret = \"KIT_WEBHOOK_SECRET\", events = [\"pull_request\"], prompt = \"Review PR #{pull_request.number}: {pull_request.title}\" }";

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let (adapter, platform) = mock::kit(home.path()).await.unwrap();
    let report = conformance::run(adapter, platform).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
}

#[tokio::test]
async fn unsigned_and_wrongly_signed_requests_are_not_verified() {
    let (_home, webhook, _link) = connected(PR_ROUTE).await;
    assert!(!webhook.verify_http(&post("pr", &[("x-github-event", "pull_request")], "{}")));
    let forged = post("pr", &[("x-hub-signature-256", "sha256=deadbeef")], "{}");
    assert!(!webhook.verify_http(&forged));
    assert!(webhook.verify_http(&signed("pull_request", "d1", "{}")));
}

#[tokio::test]
async fn an_event_outside_the_route_filter_is_ignored() {
    let (_home, webhook, link) = connected(PR_ROUTE).await;
    let answer = webhook.handle_http(&signed("push", "d1", "{}")).await;
    assert_eq!(answer.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"status": "ignored", "event": "push"})
    );
    assert!(link.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_signed_pull_request_becomes_the_rendered_prompt_once() {
    let (_home, webhook, link) = connected(PR_ROUTE).await;
    let payload = r#"{"pull_request": {"number": 42, "title": "Fix x"}}"#;
    let answer = webhook
        .handle_http(&signed("pull_request", "d-42", payload))
        .await;
    assert_eq!(answer.status, 202);
    let body: serde_json::Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["status"], "accepted");
    assert_eq!(body["delivery_id"], "d-42");
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.text, "Review PR #42: Fix x");
    assert_eq!(event.thread_id.as_deref(), Some("d-42"));
    assert_eq!(
        webhook.authorize_sender(&event).await.unwrap(),
        SenderVerdict::Allow
    );
    let again = webhook
        .handle_http(&signed("pull_request", "d-42", payload))
        .await;
    assert_eq!(again.status, 200);
    assert_eq!(link.events.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_routes_and_bad_json_are_refused() {
    let (_home, webhook, _link) = connected(TABLE).await;
    assert_eq!(
        webhook.handle_http(&post("nope", &[], "{}")).await.status,
        404
    );
    let sig = github_signature(SECRET, b"not json");
    let bad = post("kit", &[("x-hub-signature-256", &sig)], "not json");
    assert!(webhook.verify_http(&bad));
    assert_eq!(webhook.handle_http(&bad).await.status, 400);
}

#[tokio::test]
async fn an_unsigned_route_needs_a_loopback_listener() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[gateway.http]\nlisten = \"0.0.0.0:8644\"\npublic = true\n",
    )
    .unwrap();
    let setup = kit_setup(
        home.path(),
        "webhook",
        "routes.open = { insecure_no_auth = true }",
        &[],
    )
    .unwrap();
    let err = Webhook::new(&setup)
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "webhook_unsigned_public");
    assert!(
        err.message.contains("routes.open.insecure_no_auth"),
        "{}",
        err.message
    );
}
