//! The `msgraph_webhook` catalog adapter plugin (spec: platform-adapters,
//! Microsoft Graph change-notification listener; Catalog adapter set):
//! the conformance suite with Graph as the platform (notifications in,
//! replies kept in the plugin's replies file), the validation handshake,
//! clientState and source-range refusals, resource filtering, receipt
//! deduplication, and the options a public listener requires.

#![cfg(feature = "gateway")]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

const CLIENT_STATE: &str = "graph-client-state";
const SUBSCRIPTION: &str = "sub-1";

/// Graph, posting notifications whose resource is the injected text (the
/// adapter's `prompt` is `{resource}`).
struct Graph {
    adapter: Mutex<Option<String>>,
    last: Mutex<Option<Value>>,
    next: AtomicU64,
    replies: PathBuf,
}

impl Graph {
    fn new(profile_dir: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            adapter: Mutex::default(),
            last: Mutex::default(),
            next: AtomicU64::new(1),
            replies: profile_dir.join("plugin-data/msgraph_webhook/files/replies.jsonl"),
        })
    }

    fn prefix(&self) -> String {
        self.adapter
            .lock()
            .unwrap()
            .clone()
            .expect("the kit listens")
    }

    fn post(&self, body: &Value) -> u16 {
        catalog::post(
            &format!("{}/", self.prefix()),
            &[("content-type", "application/json")],
            body.to_string().as_bytes(),
        )
        .0
    }

    fn item(&self, resource: &str, state: &str) -> Value {
        let id = format!("n{}", self.next.fetch_add(1, Ordering::Relaxed));
        json!({
            "id": id,
            "subscriptionId": SUBSCRIPTION,
            "changeType": "created",
            "resource": resource,
            "clientState": state,
        })
    }
}

impl MockPlatform for Graph {
    fn dm(&self) -> (String, String) {
        (format!("msgraph:{SUBSCRIPTION}"), "msgraph".to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let item = self.item(text, CLIENT_STATE);
        let id = format!("id:{}", item["id"].as_str().unwrap());
        let body = json!({"value": [item]});
        *self.last.lock().unwrap() = Some(body.clone());
        assert_eq!(self.post(&body), 202);
        id
    }

    fn redeliver(&self) {
        let last = self.last.lock().unwrap().clone();
        if let Some(body) = last {
            assert_eq!(self.post(&body), 202);
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, _failure: Failure) {}

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sends_can_fail(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        std::fs::read_to_string(&self.replies)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|r| MockSent {
                chat_id: r["chat"].as_str().unwrap_or_default().to_owned(),
                message_id: r["id"].as_str().unwrap_or_default().to_owned(),
                text: r["text"].as_str().unwrap_or_default().to_owned(),
                buttons: Vec::new(),
                thread_id: None,
            })
            .collect()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn listen(&self, base: &str) {
        *self.adapter.lock().unwrap() = Some(base.to_owned());
    }
}

fn install(
    options: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("msgraph_webhook", options, &[]);
    catalog::secret(&kit, "MSGRAPH_CLIENT_STATE", CLIENT_STATE);
    (kit, root)
}

fn options() -> Value {
    json!({"client_state_secret": "MSGRAPH_CLIENT_STATE", "prompt": "{resource}"})
}

async fn connected(
    options: &Value,
) -> (
    Arc<Graph>,
    Arc<dyn pmt_gateway::messaging::Adapter>,
    Arc<catalog::Link>,
    tempfile::TempDir,
) {
    let (kit, root) = install(options);
    let graph = Graph::new(kit.setup().scope.dir());
    let adapter = kit.adapter().unwrap();
    graph.listen(&catalog::serve(Arc::clone(&adapter)).await);
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    (graph, adapter, link, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_msgraph_webhook_adapter_passes_the_conformance_suite() {
    let (kit, _root) = install(&options());
    let graph = Graph::new(kit.setup().scope.dir());
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&graph) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Validation handshake.
#[tokio::test(flavor = "multi_thread")]
async fn the_validation_token_is_echoed_as_plain_text() {
    let (graph, adapter, _link, _root) = connected(&options()).await;
    let url = format!("{}/?validationToken=abc", graph.prefix());
    let (status, body) = catalog::post(&url, &[], b"");
    assert_eq!((status, body.as_slice()), (200, b"abc".as_slice()));
    let (status, _) = catalog::request("GET", &format!("{}/", graph.prefix()), &[], b"");
    assert_eq!(status, 400);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forged_filtered_repeated_and_malformed_batches_are_answered_apart() {
    let mut opts = options();
    opts["accepted_resources"] = json!(["communications/onlineMeetings/*"]);
    let (graph, adapter, link, _root) = connected(&opts).await;
    let forged = graph.item("communications/onlineMeetings/m1", "wrong");
    assert_eq!(graph.post(&json!({"value": [forged]})), 403);
    let elsewhere = graph.item("users/u1/messages", CLIENT_STATE);
    assert_eq!(graph.post(&json!({"value": [elsewhere]})), 400);
    assert_eq!(graph.post(&json!({"not": "a batch"})), 400);
    let good = graph.item("communications/onlineMeetings/m1", CLIENT_STATE);
    let batch = json!({"value": [good]});
    assert_eq!(graph.post(&batch), 202);
    assert_eq!(graph.post(&batch), 202);
    let got = link.wait_for(2).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].text, "communications/onlineMeetings/m1");
    assert_eq!(got[0].sender_id, "msgraph");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sources_outside_the_allowed_ranges_are_refused() {
    let mut opts = options();
    opts["allowed_source_cidrs"] = json!(["52.96.0.0/14"]);
    let (graph, adapter, link, _root) = connected(&opts).await;
    let item = graph.item("users/u1", CLIENT_STATE);
    assert_eq!(graph.post(&json!({"value": [item]})), 403);
    let url = format!("{}/?validationToken=abc", graph.prefix());
    assert_eq!(catalog::post(&url, &[], b"").0, 403);
    assert!(link.wait_for(1).await.is_empty());
    adapter.disconnect().await;

    opts["allowed_source_cidrs"] = json!(["127.0.0.0/8"]);
    let (graph, adapter, link, _root) = connected(&opts).await;
    let item = graph.item("users/u1", CLIENT_STATE);
    assert_eq!(graph.post(&json!({"value": [item]})), 202);
    assert_eq!(link.wait_for(1).await.len(), 1);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_client_state_or_open_public_listener_refuses_to_start() {
    let (kit, _root) = catalog::install("msgraph_webhook", &options(), &[]);
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("MSGRAPH_CLIENT_STATE"),
        "{}",
        err.message
    );

    let (kit, _root) = install(&options());
    std::fs::write(
        kit.setup().scope.home().join("daemon.toml"),
        "[gateway.http]\nlisten = \"0.0.0.0:8644\"\npublic = true\n",
    )
    .unwrap();
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("allowed_source_cidrs"),
        "{}",
        err.message
    );
}
