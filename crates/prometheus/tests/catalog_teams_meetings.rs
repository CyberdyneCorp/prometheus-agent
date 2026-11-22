//! The `teams_meetings` catalog plugin (spec: platform-adapters, Teams
//! meeting summary pipeline; Catalog adapter set) against a mock Microsoft
//! Graph and incoming webhook: the conformance suite (a transcript
//! notification in, the summary out), one summary per meeting, the
//! renewal ticker, the recording fallback, delivery into the meeting chat
//! and the `prometheus plugins run teams_meetings` commands run by the
//! real binary on the same profile.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::catalog::{self, MockHttp, Reply, Request};
use common::catalog_jwt;
use common::{ok, run};
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use pmt_gateway::messaging::{MediaKind, Outgoing};
use serde_json::{Value, json};

const TENANT: &str = "tenant-1";
const CLIENT_STATE: &str = "meetings-client-state";

/// A meeting: id, transcripts (id, vtt), recordings (id, bytes).
type Meeting = (String, Vec<(String, String)>, Vec<(String, Vec<u8>)>);

#[derive(Default)]
struct State {
    adapter: Option<String>,
    /// Meetings: (id, transcripts (id, vtt), recordings (id, bytes)).
    meetings: Vec<Meeting>,
    /// Subscriptions: id -> (resource, expiry).
    subscriptions: Vec<(String, String, String)>,
    /// The expiry the mock grants new subscriptions, overriding the asked one.
    grant_expiry: Option<String>,
    patches: Vec<(String, String)>,
    webhook: Vec<String>,
    chat_posts: Vec<(String, String)>,
    last: Option<Value>,
    latest_meeting: String,
    fail_next: Option<Failure>,
}

struct Graph {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn subscriptions(state: &mut State, request: &Request, rest: &str) -> Reply {
    match (request.method.as_str(), rest) {
        ("GET", "") => {
            let value: Vec<Value> = state
                .subscriptions
                .iter()
                .map(|(id, r, e)| json!({"id": id, "resource": r, "expirationDateTime": e}))
                .collect();
            Reply::json(200, &json!({"value": value}))
        }
        ("POST", "") => {
            let body = request.json();
            let id = format!("sub-{}", state.subscriptions.len() + 1);
            let expiry = state
                .grant_expiry
                .clone()
                .unwrap_or_else(|| body["expirationDateTime"].as_str().unwrap().to_owned());
            let resource = body["resource"].as_str().unwrap().to_owned();
            assert_eq!(body["clientState"], CLIENT_STATE);
            state
                .subscriptions
                .push((id.clone(), resource.clone(), expiry.clone()));
            Reply::json(
                201,
                &json!({"id": id, "resource": resource, "expirationDateTime": expiry}),
            )
        }
        ("PATCH", id) => {
            let expiry = request.json()["expirationDateTime"]
                .as_str()
                .unwrap()
                .to_owned();
            state.patches.push((id.to_owned(), expiry.clone()));
            Reply::json(200, &json!({"id": id, "expirationDateTime": expiry}))
        }
        ("DELETE", id) => {
            state.subscriptions.retain(|(s, ..)| s != id);
            Reply::status(204)
        }
        _ => Reply::status(405),
    }
}

fn meeting_route(state: &mut State, rest: &str) -> Reply {
    let parts: Vec<&str> = rest.split('/').collect();
    let Some(meeting) = state.meetings.iter().find(|m| m.0 == parts[0]) else {
        return Reply::status(404);
    };
    match parts.as_slice() {
        [id] => Reply::json(
            200,
            &json!({"id": id, "chatInfo": {"threadId": format!("19:meeting_{id}@thread.v2")}}),
        ),
        [_, "transcripts"] => {
            let value: Vec<Value> = meeting.1.iter().map(|(t, _)| json!({"id": t})).collect();
            Reply::json(200, &json!({"value": value}))
        }
        [_, "recordings"] => {
            let value: Vec<Value> = meeting.2.iter().map(|(r, _)| json!({"id": r})).collect();
            Reply::json(200, &json!({"value": value}))
        }
        [_, "transcripts", t, "content"] => match meeting.1.iter().find(|(id, _)| id == t) {
            Some((_, vtt)) => Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/vtt".into())],
                body: vtt.clone().into_bytes(),
            },
            None => Reply::status(404),
        },
        [_, "recordings", r, "content"] => match meeting.2.iter().find(|(id, _)| id == r) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        },
        _ => Reply::status(404),
    }
}

fn webhook(state: &mut State, request: &Request) -> Reply {
    match state.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::status(429),
        Some(Failure::ServerError) => return Reply::status(503),
        Some(Failure::Forbidden | Failure::Markup) => return Reply::status(400),
        None => {}
    }
    state.webhook.push(
        request.json()["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    );
    Reply::json(200, &json!({}))
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    if route == "/hook" {
        return webhook(&mut state, request);
    }
    if route == format!("/{TENANT}/oauth2/v2.0/token") {
        assert_eq!(
            request.field("scope").as_deref(),
            Some("https://graph.microsoft.com/.default")
        );
        return Reply::json(
            200,
            &json!({"access_token": "graph-token", "expires_in": 3600}),
        );
    }
    if request.header("authorization") != Some("Bearer graph-token") {
        return Reply::status(401);
    }
    let mut reply = if let Some(rest) = route.strip_prefix("/v1.0/subscriptions") {
        subscriptions(&mut state, request, rest.trim_start_matches('/'))
    } else if let Some(rest) = route.strip_prefix("/v1.0/communications/onlineMeetings/") {
        meeting_route(&mut state, rest)
    } else if let Some(rest) = route.strip_prefix("/v1.0/chats/") {
        let thread = rest.trim_end_matches("/messages").to_owned();
        let text = request.json()["body"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        state.chat_posts.push((thread, text));
        Reply::json(
            201,
            &json!({"id": format!("chatmsg-{}", state.chat_posts.len())}),
        )
    } else {
        Reply::status(404)
    };
    reply.headers.push(catalog_jwt::date_header());
    reply
}

impl Graph {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let api = MockHttp::start(Arc::new(move |request: &Request| answer(&served, request)));
        Arc::new(Self {
            api,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self) -> Value {
        json!({
            "client_id": "graph-app",
            "tenant_id": TENANT,
            "public_url": "https://agent.example/p/default/teams_meetings",
            "graph_base": format!("{}/v1.0", self.api.base()),
            "login_base": self.api.base(),
            "summary_prompt": "",
        })
    }

    fn meeting(&self, transcript: Option<&str>, recording: Option<&[u8]>) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let id = format!("M{n}");
        let transcripts = transcript
            .map(|t| {
                vec![(
                    format!("T{n}"),
                    format!("WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n{t}\n"),
                )]
            })
            .unwrap_or_default();
        let recordings = recording
            .map(|r| vec![(format!("R{n}"), r.to_vec())])
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        state.meetings.push((id.clone(), transcripts, recordings));
        state.latest_meeting.clone_from(&id);
        id
    }

    /// Posts a notification for `resource`: its receipt id.
    fn notify(&self, resource: &str) -> String {
        let n = format!("n{}", self.next.fetch_add(1, Ordering::Relaxed));
        let body = json!({"value": [{
            "id": n,
            "subscriptionId": "sub-1",
            "changeType": "created",
            "clientState": CLIENT_STATE,
            "resource": resource,
        }]});
        let prefix = self
            .state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens");
        self.state.lock().unwrap().last = Some(body.clone());
        let (status, _) = catalog::post(
            &format!("{prefix}/"),
            &[("content-type", "application/json")],
            body.to_string().as_bytes(),
        );
        assert_eq!(status, 202);
        format!("id:{n}")
    }

    fn transcript_of(meeting: &str) -> String {
        format!(
            "communications/onlineMeetings('{meeting}')/transcripts('T{}')",
            &meeting[1..]
        )
    }
}

impl MockPlatform for Graph {
    fn dm(&self) -> (String, String) {
        let latest = self.state.lock().unwrap().latest_meeting.clone();
        (format!("meeting:{latest}"), "teams_meetings".to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let meeting = self.meeting(Some(text), None);
        self.notify(&Graph::transcript_of(&meeting))
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        let prefix = self.state.lock().unwrap().adapter.clone().unwrap();
        if let Some(body) = last {
            catalog::post(&format!("{prefix}/"), &[], body.to_string().as_bytes());
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.state.lock().unwrap().fail_next = Some(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn hints_retry_after(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.state
            .lock()
            .unwrap()
            .webhook
            .iter()
            .enumerate()
            .map(|(i, text)| MockSent {
                chat_id: String::new(),
                message_id: format!("hook-{i}"),
                text: text.clone(),
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
        self.state.lock().unwrap().adapter = Some(base.to_owned());
    }
}

fn install(
    mock: &Graph,
    options: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("teams_meetings", options, &[mock.api.host()]);
    catalog::secret(&kit, "MSGRAPH_CLIENT_SECRET", "graph-secret");
    catalog::secret(&kit, "MSGRAPH_CLIENT_STATE", CLIENT_STATE);
    catalog::secret(
        &kit,
        "TEAMS_MEETINGS_WEBHOOK_URL",
        &format!("{}/hook", mock.api.base()),
    );
    (kit, root)
}

async fn connected(
    mock: &Arc<Graph>,
    kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit,
) -> (Arc<dyn pmt_gateway::messaging::Adapter>, Arc<catalog::Link>) {
    let adapter = kit.adapter().unwrap();
    mock.listen(&catalog::serve(Arc::clone(&adapter)).await);
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    (adapter, link)
}

/// `prometheus plugins run teams_meetings <args>` on the kit's home.
fn cli(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["plugins", "run", "teams_meetings"];
    all.extend(args);
    ok(&run(home, &all))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_teams_meetings_pipeline_passes_the_conformance_suite() {
    let mock = Graph::start();
    let (kit, _root) = install(&mock, &mock.options());
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Duplicate notification summarized once.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_notification_for_a_summarized_transcript_is_a_duplicate() {
    let mock = Graph::start();
    let (kit, _root) = install(&mock, &mock.options());
    let (adapter, link) = connected(&mock, &kit).await;
    let meeting = mock.meeting(Some("<v Ada>Ship on Friday.</v>"), None);
    let resource = Graph::transcript_of(&meeting);
    mock.notify(&resource);
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].chat_id, format!("meeting:{meeting}"));
    assert_eq!(got[0].text, "Ada: Ship on Friday.");
    adapter
        .send(&Outgoing::text(
            &got[0].chat_id,
            None,
            "Decision: ship Friday.",
        ))
        .await
        .unwrap();
    let again = mock.notify(&resource);
    assert_eq!(link.wait_for(2).await.len(), 1);
    assert_eq!(
        mock.state.lock().unwrap().webhook,
        ["Decision: ship Friday."]
    );
    let status = cli(kit.setup().scope.home(), &["status"]);
    let entry = status
        .lines()
        .find(|l| l.contains(&again))
        .unwrap_or_else(|| panic!("{status}"));
    assert!(entry.ends_with("duplicate"), "{entry}");
    assert!(
        status.contains(&format!("{meeting}  summarized")),
        "{status}"
    );
    // A replay asked from the command line summarizes it once more.
    cli(kit.setup().scope.home(), &["replay", &meeting]);
    let got = link.wait_for(2).await;
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].text, "Ada: Ship on Friday.");
    adapter.disconnect().await;
}

/// Scenario: Subscription renewed before expiry.
#[tokio::test(flavor = "multi_thread")]
async fn the_ticker_renews_a_subscription_expiring_soon() {
    let mock = Graph::start();
    let soon = jiff::Timestamp::now() + jiff::SignedDuration::from_mins(30);
    let soon = soon.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    mock.state.lock().unwrap().grant_expiry = Some(soon.clone());
    let mut options = mock.options();
    options["renew_check_secs"] = json!(1);
    let (kit, _root) = install(&mock, &options);
    let home = kit.setup().scope.home().to_path_buf();
    let subscribed = cli(&home, &["subscribe"]);
    assert!(
        subscribed.contains(&format!("expires {soon}")),
        "{subscribed}"
    );
    let (adapter, _link) = connected(&mock, &kit).await;
    let mut patches = Vec::new();
    for _ in 0..100 {
        patches = mock.state.lock().unwrap().patches.clone();
        if !patches.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (id, renewed) = patches.first().cloned().expect("no renewal");
    assert_eq!(id, "sub-1");
    assert!(renewed > soon, "{renewed} is not after {soon}");
    adapter.disconnect().await;
    let listed = cli(&home, &["list"]);
    assert!(listed.contains(&format!("expires {renewed}")), "{listed}");
    // Removing deletes it at Graph and from the record.
    cli(&home, &["remove", "sub-1"]);
    assert!(mock.state.lock().unwrap().subscriptions.is_empty());
    assert!(cli(&home, &["list"]).contains("no subscriptions"));
}

#[tokio::test(flavor = "multi_thread")]
async fn recordings_stand_in_only_when_allowed_and_graph_delivery_posts_in_the_meeting_chat() {
    let mock = Graph::start();
    let mut options = mock.options();
    options["delivery"] = json!("graph");
    let (kit, _root) = install(&mock, &options);
    let (adapter, link) = connected(&mock, &kit).await;
    let meeting = mock.meeting(None, Some(b"recorded audio"));
    mock.notify(&format!(
        "communications/onlineMeetings('{meeting}')/recordings('R1')"
    ));
    // A transcript that is not there yet fails without the fallback.
    mock.notify(&Graph::transcript_of(&meeting));
    assert!(link.wait_for(1).await.is_empty());
    let status = cli(kit.setup().scope.home(), &["status"]);
    assert!(
        status.contains("ignored") && status.contains("failed"),
        "{status}"
    );
    // Graph delivery goes into the meeting's chat.
    let summarized = mock.meeting(Some("Hello all"), None);
    mock.notify(&Graph::transcript_of(&summarized));
    let got = link.wait_for(1).await;
    adapter
        .send(&Outgoing::text(&got[0].chat_id, None, "Summary text"))
        .await
        .unwrap();
    let posts = mock.state.lock().unwrap().chat_posts.clone();
    assert_eq!(
        posts,
        [(
            format!("19:meeting_{summarized}@thread.v2"),
            "Summary text".to_owned()
        )]
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_transcript_the_recording_stands_in_when_allowed() {
    let mock = Graph::start();
    let mut options = mock.options();
    options["use_recordings"] = json!(true);
    let (kit, _root) = install(&mock, &options);
    let (adapter, link) = connected(&mock, &kit).await;
    let meeting = mock.meeting(None, Some(b"recorded audio"));
    mock.notify(&Graph::transcript_of(&meeting));
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0].text,
        "(The meeting has no transcript: its recording is attached.)"
    );
    let [attachment] = got[0].attachments.as_slice() else {
        panic!("{:?}", got[0].attachments);
    };
    assert_eq!(attachment.kind, MediaKind::Audio);
    adapter.disconnect().await;
}
