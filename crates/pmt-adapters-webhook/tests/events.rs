//! Event coalescing and MCP event routes (spec: platform-adapters, Webhook
//! event coalescing; MCP server events into the webhook pipeline).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use pmt_adapters_webhook::mock::{SECRET, SECRET_NAME};
use pmt_adapters_webhook::{Webhook, github_signature};
use pmt_config::mcp_event_state;
use pmt_config::schema::{McpEventRoute, SecretName, WebhookRoute};
use pmt_config::webhook_subscriptions::{self, Subscription};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::sign::hmac_sha256;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterSetup, AdapterState, Admission,
    HttpRequest, HttpResponse, Inbound,
};
use serde_json::{Value, json};

#[derive(Default)]
struct Gateway {
    events: Mutex<Vec<Inbound>>,
    delivered: Mutex<Vec<(String, String)>>,
}

impl AdapterLink for Gateway {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}

    fn deliver<'a>(
        &'a self,
        target: &'a str,
        text: &'a str,
    ) -> AdapterFuture<'a, Result<String, String>> {
        self.delivered
            .lock()
            .unwrap()
            .push((target.to_owned(), text.to_owned()));
        Box::pin(async move { Ok(target.to_owned()) })
    }
}

struct Harness {
    _home: tempfile::TempDir,
    setup: AdapterSetup,
    webhook: Arc<Webhook>,
    gateway: Arc<Gateway>,
}

async fn harness(table: &str, prepare: impl FnOnce(&AdapterSetup)) -> Harness {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let setup = kit_setup(home.path(), "webhook", table, &[(SECRET_NAME, SECRET)]).unwrap();
    prepare(&setup);
    let webhook = Arc::new(Webhook::new(&setup));
    let gateway = Arc::new(Gateway::default());
    webhook
        .connect(Arc::clone(&gateway) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    Harness {
        _home: home,
        setup,
        webhook,
        gateway,
    }
}

fn json_of(answer: &HttpResponse) -> Value {
    serde_json::from_slice(&answer.body).unwrap()
}

fn github(route: &str, delivery: &str, body: &Value) -> HttpRequest {
    let body = body.to_string();
    HttpRequest {
        method: "POST".into(),
        path: format!("/{route}"),
        query: None,
        headers: vec![
            ("x-github-event".into(), "push".into()),
            ("x-github-delivery".into(), delivery.into()),
            (
                "x-hub-signature-256".into(),
                github_signature(SECRET, body.as_bytes()),
            ),
        ],
        body: body.into_bytes(),
    }
}

/// Spec: platform-adapters, Webhook event coalescing, "Burst of pushes".
#[tokio::test(start_paused = true)]
async fn a_burst_of_pushes_runs_one_turn_on_the_last_payload() {
    let h = harness(
        "routes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\", deliver = \"log\", prompt = \"push {after}\", coalesce = { key = \"payload.ref\", window_secs = 30 } }",
        |_| {},
    )
    .await;
    for n in 1..=5 {
        let body = json!({"ref": "refs/heads/main", "after": format!("c{n}")});
        let answer = h
            .webhook
            .handle_http(&github("ci", &format!("d{n}"), &body))
            .await;
        assert_eq!(answer.status, 202);
        assert_eq!(json_of(&answer)["status"], "coalesced");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    assert!(h.gateway.events.lock().unwrap().is_empty());
    let again = h
        .webhook
        .handle_http(&github("ci", "d5", &json!({"ref": "refs/heads/main"})))
        .await;
    assert_eq!(json_of(&again)["status"], "duplicate", "idempotency first");
    tokio::time::sleep(Duration::from_secs(31)).await;
    let events = h.gateway.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].text.starts_with("push c5"), "{}", events[0].text);
    assert!(
        events[0].text.contains("4 earlier events"),
        "{}",
        events[0].text
    );
}

/// An event whose key does not resolve is not held, and stopping the
/// adapter flushes what is held.
#[tokio::test(start_paused = true)]
async fn an_unresolved_key_dispatches_at_once_and_stop_flushes() {
    let h = harness(
        "routes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\", deliver = \"log\", coalesce = { key = \"pull_request.number\" } }",
        |_| {},
    )
    .await;
    let now = h
        .webhook
        .handle_http(&github("ci", "d1", &json!({"ref": "x"})))
        .await;
    assert_eq!(json_of(&now)["status"], "accepted");
    let held = json!({"pull_request": {"number": 3}});
    let answer = h.webhook.handle_http(&github("ci", "d2", &held)).await;
    assert_eq!(json_of(&answer)["status"], "coalesced");
    h.webhook.disconnect().await;
    assert_eq!(h.gateway.events.lock().unwrap().len(), 2);
}

const MCP_SECRET_NAME: &str = "WEBHOOK_MCP_GH";

fn mcp_secret() -> String {
    format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    )
}

fn subscribe(setup: &AdapterSetup, deliver: &str) {
    setup
        .scope
        .secrets()
        .store()
        .set(
            MCP_SECRET_NAME,
            &pmt_core::secret::SecretString::new(mcp_secret()),
        )
        .unwrap();
    let route = WebhookRoute {
        secret_secret: Some(SecretName(MCP_SECRET_NAME.to_owned())),
        prompt: Some("MCP event {eventId} from gh:\n{data}\nSummarize.".to_owned()),
        deliver: Some(deliver.to_owned()),
        mcp_event: Some(McpEventRoute {
            server: "gh".into(),
            name: "issue.opened".into(),
            arguments: "{}".into(),
        }),
        ..WebhookRoute::default()
    };
    let mut all = BTreeMap::new();
    all.insert(
        "mcp-gh".to_owned(),
        Subscription {
            created_at: "2026-11-21T00:00:00Z".into(),
            generated_secret: true,
            route,
        },
    );
    webhook_subscriptions::save(setup.scope.dir(), &all).unwrap();
}

/// A Standard Webhooks request on the MCP route (`attempt` is the
/// per-attempt `webhook-id`).
fn standard(attempt: &str, body: &Value) -> HttpRequest {
    let body = body.to_string();
    let ts = jiff::Timestamp::now().as_second().to_string();
    let key = base64::engine::general_purpose::STANDARD
        .decode(&mcp_secret()[6..])
        .unwrap();
    let signed = format!("{attempt}.{ts}.{body}");
    let signature =
        base64::engine::general_purpose::STANDARD.encode(hmac_sha256(&key, signed.as_bytes()));
    HttpRequest {
        method: "POST".into(),
        path: "/mcp-gh".into(),
        query: None,
        headers: vec![
            ("webhook-id".into(), attempt.into()),
            ("webhook-timestamp".into(), ts),
            ("webhook-signature".into(), format!("v1,{signature}")),
        ],
        body: body.into_bytes(),
    }
}

async fn mcp(deliver: &str) -> Harness {
    harness("", |setup| subscribe(setup, deliver)).await
}

/// Spec: platform-adapters, MCP server events into the webhook pipeline,
/// "Placeholder injection inert"; plugin-system, MCP events, "Duplicate
/// event ignored" (the receiving side: the second delivery of one
/// `eventId` starts nothing).
#[tokio::test]
async fn event_data_is_inert_and_a_redelivered_event_id_runs_once() {
    let h = mcp("log").await;
    let body =
        json!({"eventId": "e1", "cursor": "c-1", "data": {"title": "{__raw__} and {eventId}"}});
    let first = standard("attempt-1", &body);
    assert!(h.webhook.verify_http(&first));
    assert_eq!(h.webhook.handle_http(&first).await.status, 202);
    let retry = standard("attempt-2", &body);
    assert!(h.webhook.verify_http(&retry));
    assert_eq!(
        json_of(&h.webhook.handle_http(&retry).await)["status"],
        "duplicate"
    );
    let events = h.gateway.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let text = &events[0].text;
    assert!(text.starts_with("MCP event e1 from gh:"), "{text}");
    assert!(text.contains("{__raw__} and {eventId}"), "{text}");
    assert!(
        !text.contains("\"cursor\""),
        "the raw payload never enters: {text}"
    );
    assert_eq!(events[0].message_id, "mcp-event:e1");
    let state = mcp_event_state::get(h.setup.scope.dir(), "mcp-gh");
    assert_eq!(state.cursor.as_deref(), Some("c-1"));
}

#[tokio::test]
async fn control_envelopes_are_answered_without_a_turn() {
    let h = mcp("log").await;
    let ask = |body: Value| {
        let webhook = Arc::clone(&h.webhook);
        async move { webhook.handle_http(&standard("a", &body)).await }
    };
    let echo = ask(json!({"type": "verification", "challenge": "abc"})).await;
    assert_eq!(
        (echo.status, json_of(&echo)),
        (200, json!({"challenge": "abc"}))
    );
    let long = ask(json!({"type": "verification", "challenge": "x".repeat(513)})).await;
    assert_eq!(long.status, 400);
    let gap = ask(json!({"type": "gap", "cursor": "fresh"})).await;
    assert_eq!(json_of(&gap), json!({"status": "gap"}));
    let unknown = ask(json!({"type": "later.kind"})).await;
    assert_eq!(
        json_of(&unknown),
        json!({"status": "ignored", "type": "later.kind"})
    );
    assert!(h.gateway.events.lock().unwrap().is_empty());
    let state = mcp_event_state::get(h.setup.scope.dir(), "mcp-gh");
    assert_eq!(state.cursor.as_deref(), Some("fresh"));
}

#[tokio::test]
async fn termination_removes_the_route_and_tells_its_target() {
    let h = mcp("telegram:42").await;
    let body =
        json!({"type": "terminated", "error": {"code": -32024, "message": "access revoked"}});
    let answer = h.webhook.handle_http(&standard("a", &body)).await;
    assert_eq!(json_of(&answer), json!({"status": "terminated"}));
    assert!(
        webhook_subscriptions::load(h.setup.scope.dir())
            .unwrap()
            .is_empty()
    );
    assert!(h.setup.scope.secrets().resolve(MCP_SECRET_NAME).is_err());
    let delivered = h.gateway.delivered.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, "telegram:42");
    assert!(
        delivered[0].1.contains("access revoked"),
        "{}",
        delivered[0].1
    );
}

#[tokio::test]
async fn an_mcp_route_accepts_only_standard_webhooks_signatures() {
    let h = mcp("log").await;
    let body = json!({"eventId": "e9", "data": {}});
    let mut wrong = standard("a", &body);
    wrong.headers.retain(|(k, _)| k != "webhook-signature");
    let text = body.to_string();
    wrong.headers.push((
        "x-hub-signature-256".into(),
        github_signature(&mcp_secret(), text.as_bytes()),
    ));
    assert!(!h.webhook.verify_http(&wrong));
}
