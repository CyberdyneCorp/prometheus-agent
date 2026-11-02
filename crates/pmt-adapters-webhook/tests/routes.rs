//! Webhook route behaviour (spec: platform-adapters, Webhook limits,
//! idempotency and responses; Webhook payload filters and transform
//! scripts; Webhook direct delivery mode), on signed fixtures.

use std::sync::{Arc, Mutex};

use pmt_adapters_webhook::mock::{SECRET, SECRET_NAME};
use pmt_adapters_webhook::{Webhook, github_signature};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, HttpRequest,
    HttpResponse, Inbound,
};
use serde_json::{Value, json};

/// A gateway that admits every event (no dedup of its own) and records
/// direct deliveries.
#[derive(Default)]
struct Gateway {
    events: Mutex<Vec<Inbound>>,
    delivered: Mutex<Vec<(String, String)>>,
    refuse_delivery: bool,
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
        Box::pin(async move {
            if self.refuse_delivery {
                return Err("no home channel".to_owned());
            }
            self.delivered
                .lock()
                .unwrap()
                .push((target.to_owned(), text.to_owned()));
            Ok(format!("{target}:home"))
        })
    }
}

fn signed(route: &str, event: &str, delivery: &str, body: &Value) -> HttpRequest {
    let body = body.to_string();
    let sig = github_signature(SECRET, body.as_bytes());
    HttpRequest {
        method: "POST".into(),
        path: format!("/{route}"),
        query: None,
        headers: vec![
            ("x-github-event".into(), event.into()),
            ("x-github-delivery".into(), delivery.into()),
            ("x-hub-signature-256".into(), sig),
        ],
        body: body.into_bytes(),
    }
}

fn json_of(answer: &HttpResponse) -> Value {
    serde_json::from_slice(&answer.body).unwrap()
}

struct Route {
    _home: tempfile::TempDir,
    webhook: Webhook,
    gateway: Arc<Gateway>,
}

impl Route {
    async fn new(table: &str) -> Self {
        Self::with(table, Gateway::default(), |_| {}).await
    }

    async fn with(table: &str, gateway: Gateway, prepare: impl FnOnce(&std::path::Path)) -> Self {
        let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let setup = kit_setup(home.path(), "webhook", table, &[(SECRET_NAME, SECRET)]).unwrap();
        prepare(setup.scope.dir());
        let webhook = Webhook::new(&setup);
        let gateway = Arc::new(gateway);
        webhook
            .connect(Arc::clone(&gateway) as Arc<dyn AdapterLink>)
            .await
            .unwrap();
        Self {
            _home: home,
            webhook,
            gateway,
        }
    }

    async fn post(&self, route: &str, event: &str, delivery: &str, body: &Value) -> HttpResponse {
        let request = signed(route, event, delivery, body);
        assert!(self.webhook.verify_http(&request));
        self.webhook.handle_http(&request).await
    }

    fn events(&self) -> Vec<Inbound> {
        self.gateway.events.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn a_redelivery_within_the_hour_starts_no_second_turn() {
    let route = Route::new("routes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\" }").await;
    let payload = json!({"ref": "main"});
    let first = route.post("ci", "push", "gh-1", &payload).await;
    assert_eq!(first.status, 202);
    let again = route.post("ci", "push", "gh-1", &payload).await;
    assert_eq!(again.status, 200);
    assert_eq!(json_of(&again), json!({"status": "duplicate"}));
    assert_eq!(route.events().len(), 1);
    // Another delivery of the same payload is a new event.
    assert_eq!(route.post("ci", "push", "gh-2", &payload).await.status, 202);
    assert_eq!(route.events().len(), 2);
}

#[tokio::test]
async fn only_failing_ci_runs_pass_the_filter() {
    let route = Route::new(
        "routes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\", filters = { equals = { path = \"payload.workflow_run.conclusion\", value = \"failure\" } } }",
    )
    .await;
    let success = json!({"workflow_run": {"conclusion": "success"}});
    let answer = route.post("ci", "workflow_run", "r1", &success).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        json_of(&answer),
        json!({"status": "ignored", "reason": "filter", "route": "ci"})
    );
    assert!(route.events().is_empty());
    let failure = json!({"workflow_run": {"conclusion": "failure"}});
    assert_eq!(
        route
            .post("ci", "workflow_run", "r2", &failure)
            .await
            .status,
        202
    );
    assert_eq!(route.events().len(), 1);
}

#[tokio::test]
async fn the_route_rate_limit_answers_429() {
    let route = Route::new(
        "routes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\", rate_limit_per_min = 2 }",
    )
    .await;
    for n in 0..2 {
        let answer = route.post("ci", "push", &format!("d{n}"), &json!({})).await;
        assert_eq!(answer.status, 202);
    }
    assert_eq!(route.post("ci", "push", "d9", &json!({})).await.status, 429);
    assert_eq!(route.events().len(), 2);
}

#[tokio::test]
async fn a_deliver_only_route_forwards_the_rendered_text_without_a_turn() {
    let table = "routes.alert = { secret_secret = \"KIT_WEBHOOK_SECRET\", deliver_only = true, deliver = \"telegram\", skills = [\"triage\"], prompt = \"ALERT {alert.name} on {alert.host}\" }";
    let route = Route::new(table).await;
    let payload = json!({"alert": {"name": "disk full", "host": "db1"}});
    let answer = route.post("alert", "alert", "a1", &payload).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        json_of(&answer),
        json!({"status": "delivered", "route": "alert", "target": "telegram", "delivery_id": "a1"})
    );
    assert!(route.events().is_empty(), "no turn starts");
    assert_eq!(
        route.gateway.delivered.lock().unwrap().clone(),
        vec![("telegram".to_owned(), "ALERT disk full on db1".to_owned())]
    );
    assert!(
        route.webhook.session_hints("alert", None).is_empty(),
        "the route's skills are ignored"
    );

    let failing = Gateway {
        refuse_delivery: true,
        ..Gateway::default()
    };
    let route = Route::with(table, failing, |_| {}).await;
    let answer = route.post("alert", "alert", "a2", &payload).await;
    assert_eq!(answer.status, 502);
    assert_eq!(json_of(&answer), json!({"error": "Delivery failed"}));
}

#[tokio::test]
async fn route_skills_and_toolsets_shape_the_delivery_sessions() {
    let route = Route::new(
        "routes.pr = { secret_secret = \"KIT_WEBHOOK_SECRET\", skills = [\"review\"], toolsets = [\"web\"] }",
    )
    .await;
    let hints = route.webhook.session_hints("pr", Some("delivery-1"));
    assert_eq!(hints.skills, vec!["review"]);
    assert_eq!(hints.toolsets, Some(vec!["web".to_owned()]));
    assert!(route.webhook.session_hints("other", None).is_empty());
}

#[test]
fn the_body_limit_follows_the_platform_table() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let setup = kit_setup(
        home.path(),
        "webhook",
        "max_body_bytes = 4096\nroutes.ci = { secret_secret = \"KIT_WEBHOOK_SECRET\" }",
        &[(SECRET_NAME, SECRET)],
    )
    .unwrap();
    assert_eq!(Webhook::new(&setup).max_body_bytes(), 4096);
    let setup = kit_setup(home.path(), "webhook", "", &[]).unwrap();
    assert_eq!(Webhook::new(&setup).max_body_bytes(), 1_048_576);
}

#[cfg(unix)]
mod scripts {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn script(dir: &std::path::Path, name: &str, body: &str) {
        let scripts = dir.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let file = scripts.join(name);
        std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn route_with(name: &str) -> String {
        format!(
            "routes.ci = {{ secret_secret = \"KIT_WEBHOOK_SECRET\", script = \"{name}\", script_timeout_secs = 2, prompt = \"{{summary}}|{{script_output}}\" }}"
        )
    }

    #[tokio::test]
    async fn a_json_object_from_the_script_replaces_the_payload() {
        let route = Route::with(&route_with("t.sh"), Gateway::default(), |dir| {
            script(
                dir,
                "t.sh",
                "cat >/dev/null; echo '{\"summary\": \"build 7 failed\"}'",
            );
        })
        .await;
        assert_eq!(route.post("ci", "push", "s1", &json!({})).await.status, 202);
        assert_eq!(route.events()[0].text, "build 7 failed|{script_output}");
    }

    #[tokio::test]
    async fn plain_output_is_added_as_script_output() {
        let route = Route::with(&route_with("t.sh"), Gateway::default(), |dir| {
            script(dir, "t.sh", "cat >/dev/null; echo 'look at db1'");
        })
        .await;
        let payload = json!({"summary": "alert"});
        assert_eq!(route.post("ci", "push", "s1", &payload).await.status, 202);
        assert_eq!(route.events()[0].text, "alert|look at db1");
    }

    #[tokio::test]
    async fn silence_failure_timeout_and_missing_scripts_drop_the_event() {
        let cases = [
            ("silent.sh", "echo '[SILENT]'"),
            ("ignore.sh", "echo '{\"ignore\": true}'"),
            ("empty.sh", "cat >/dev/null"),
            ("fail.sh", "echo '{\"summary\": \"x\"}'; exit 3"),
            ("slow.sh", "sleep 10"),
        ];
        for (name, body) in cases {
            let route = Route::with(&route_with(name), Gateway::default(), |dir| {
                script(dir, name, body);
            })
            .await;
            let answer = route.post("ci", "push", "s1", &json!({})).await;
            assert_eq!(answer.status, 200, "{name}");
            assert_eq!(
                json_of(&answer),
                json!({"status": "ignored", "reason": "script"}),
                "{name}"
            );
            assert!(route.events().is_empty(), "{name}");
        }
        let route = Route::new(&route_with("absent.sh")).await;
        let answer = route.post("ci", "push", "s1", &json!({})).await;
        assert_eq!(json_of(&answer)["reason"], "script");
    }
}
