//! Where a webhook run's answer goes (spec: messaging-gateway, Webhook
//! route delivery and safe default toolset): back to the HTTP caller, to
//! send targets (mirrored into their sessions), to GitHub, and the
//! read-only default toolset.

use std::sync::{Arc, Mutex};

use pmt_adapters_webhook::mock::{SECRET, SECRET_NAME};
use pmt_adapters_webhook::{Webhook, github_signature};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, HttpRequest,
    HttpResponse, Inbound, Outgoing, mock_server,
};
use serde_json::{Value, json};

/// A gateway that admits every event and records deliveries and mirrors.
#[derive(Default)]
struct Gateway {
    events: Mutex<Vec<Inbound>>,
    delivered: Mutex<Vec<(String, String)>>,
    mirrored: Mutex<Vec<(String, String)>>,
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

    fn mirror<'a>(&'a self, target: &'a str, text: &'a str) -> AdapterFuture<'a, ()> {
        self.mirrored
            .lock()
            .unwrap()
            .push((target.to_owned(), text.to_owned()));
        Box::pin(async {})
    }
}

struct Route {
    home: tempfile::TempDir,
    webhook: Arc<Webhook>,
    gateway: Arc<Gateway>,
}

async fn route(table: &str, secrets: &[(&str, &str)]) -> Route {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut all = vec![(SECRET_NAME, SECRET)];
    all.extend_from_slice(secrets);
    let setup = kit_setup(home.path(), "webhook", table, &all).unwrap();
    let webhook = Arc::new(Webhook::new(&setup));
    let gateway = Arc::new(Gateway::default());
    webhook
        .connect(Arc::clone(&gateway) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    Route {
        home,
        webhook,
        gateway,
    }
}

fn signed(route: &str, delivery: &str, body: &Value) -> HttpRequest {
    let body = body.to_string();
    HttpRequest {
        method: "POST".into(),
        path: format!("/{route}"),
        query: None,
        headers: vec![
            ("x-github-event".into(), "pull_request".into()),
            ("x-github-delivery".into(), delivery.into()),
            (
                "x-hub-signature-256".into(),
                github_signature(SECRET, body.as_bytes()),
            ),
        ],
        body: body.into_bytes(),
    }
}

fn json_of(answer: &HttpResponse) -> Value {
    serde_json::from_slice(&answer.body).unwrap()
}

/// The run's answer, as the gateway sends it to the delivery's thread.
fn answer(route: &str, delivery: &str, text: &str) -> Outgoing {
    Outgoing::text(route, Some(delivery), text)
}

impl Route {
    /// Posts a request and, once its turn was submitted, answers it.
    async fn ask(
        &self,
        name: &str,
        delivery: &str,
        body: &Value,
        reply: Option<&str>,
    ) -> HttpResponse {
        let webhook = Arc::clone(&self.webhook);
        let request = signed(name, delivery, body);
        let posted = tokio::spawn(async move { webhook.handle_http(&request).await });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.gateway.events.lock().unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "no turn submitted");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if let Some(reply) = reply {
            self.webhook
                .send(&answer(name, delivery, reply))
                .await
                .unwrap();
        }
        posted.await.unwrap()
    }
}

#[tokio::test]
async fn by_default_the_http_caller_gets_the_answer() {
    let r = route(
        "routes.ask = { secret_secret = \"KIT_WEBHOOK_SECRET\" }",
        &[],
    )
    .await;
    let response = r
        .ask("ask", "d1", &json!({"q": "hi"}), Some("the answer"))
        .await;
    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["answer"], "the answer");
    assert_eq!(body["delivery_id"], "d1");
}

#[tokio::test]
async fn a_slow_run_is_accepted_and_its_answer_is_logged() {
    let r = route(
        "routes.ask = { secret_secret = \"KIT_WEBHOOK_SECRET\", origin_wait_secs = 1 }",
        &[],
    )
    .await;
    let response = r.ask("ask", "d2", &json!({}), None).await;
    assert_eq!(response.status, 202);
    assert_eq!(json_of(&response)["status"], "accepted");
    r.webhook
        .send(&answer("ask", "d2", "late answer"))
        .await
        .unwrap();
    let log = std::fs::read_to_string(
        r.home
            .path()
            .join("profiles/default/logs/webhook/ask.jsonl"),
    )
    .unwrap();
    assert!(log.contains("late answer"), "{log}");
}

/// Spec scenario: Deliver to Discord.
#[tokio::test]
async fn a_send_target_gets_the_answer_mirrored_and_the_extras_a_copy() {
    let r = route(
        "routes.ops = { secret_secret = \"KIT_WEBHOOK_SECRET\", deliver = \"discord:#ops\", deliver_extra = [\"telegram\"], mirror_to_session = true }",
        &[],
    )
    .await;
    let response = r.ask("ops", "d3", &json!({}), None).await;
    assert_eq!(response.status, 202, "the run is acknowledged at once");
    r.webhook
        .send(&answer("ops", "d3", "build failed on main"))
        .await
        .unwrap();
    let delivered = r.gateway.delivered.lock().unwrap().clone();
    assert_eq!(
        delivered,
        [
            ("discord:#ops".to_owned(), "build failed on main".to_owned()),
            ("telegram".to_owned(), "build failed on main".to_owned())
        ]
    );
    let mirrored = r.gateway.mirrored.lock().unwrap().clone();
    assert_eq!(mirrored[0].0, "discord:#ops");
    assert_eq!(
        mirrored[0].1,
        "[Webhook delivery: ops]\nbuild failed on main"
    );
}

#[tokio::test]
async fn github_comment_posts_on_the_payloads_pull_request() {
    let posted: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
    let seen = Arc::clone(&posted);
    let github = mock_server::serve(move |request: HttpRequest| {
        let seen = Arc::clone(&seen);
        async move {
            let auth = request
                .header("authorization")
                .unwrap_or_default()
                .to_owned();
            let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            seen.lock()
                .unwrap()
                .push((request.path.clone(), auth, body));
            mock_server::json(201, &json!({"id": 1}))
        }
    })
    .await
    .unwrap();
    let table = format!(
        "options = {{ github_api_url = \"http://{github}\" }}\n\
         routes.pr = {{ secret_secret = \"KIT_WEBHOOK_SECRET\", deliver = \"github_comment\", github_token_secret = \"GH_TOKEN\" }}"
    );
    let r = route(&table, &[("GH_TOKEN", "ghp_kit")]).await;
    let payload = json!({"repository": {"full_name": "acme/app"}, "pull_request": {"number": 42}});
    r.ask("pr", "d4", &payload, None).await;
    r.webhook
        .send(&answer("pr", "d4", "Looks good."))
        .await
        .unwrap();
    let posted = posted.lock().unwrap().clone();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, "/repos/acme/app/issues/42/comments");
    assert_eq!(posted[0].1, "Bearer ghp_kit");
    assert_eq!(posted[0].2["body"], "Looks good.");
}

/// Spec scenario: Default toolset is read-only (the session a delivery
/// starts gets `webhook-safe` unless the route names toolsets).
#[tokio::test]
async fn deliveries_run_read_only_unless_the_route_names_toolsets() {
    let r = route(
        "routes.ask = { secret_secret = \"KIT_WEBHOOK_SECRET\" }",
        &[],
    )
    .await;
    let hints = r.webhook.session_hints("ask", Some("d5"));
    assert_eq!(hints.toolsets, Some(vec!["webhook-safe".to_owned()]));
}

#[tokio::test]
async fn an_internal_event_needs_no_signature() {
    let r = route("routes.mcp = { secret_secret = \"KIT_WEBHOOK_SECRET\", deliver = \"log\", events = [\"resource.updated\"] }", &[]).await;
    let request = HttpRequest {
        method: "POST".into(),
        path: "/mcp".into(),
        query: None,
        headers: vec![
            ("x-prometheus-event".into(), "resource.updated".into()),
            ("x-request-id".into(), "mcp-1".into()),
        ],
        body: br#"{"uri": "file:///notes.md"}"#.to_vec(),
    };
    assert!(!r.webhook.verify_http(&request), "never admitted over HTTP");
    let response = r.webhook.handle_http(&request).await;
    assert_eq!(response.status, 202);
    assert_eq!(r.gateway.events.lock().unwrap().len(), 1);
}
