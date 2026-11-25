//! The `google_chat` catalog adapter plugin (spec: platform-adapters,
//! Google Chat adapter; Catalog adapter set) against a mock Google (token
//! endpoint checking the service account's signed assertion, Pub/Sub,
//! the Chat API and its media and upload endpoints, ID-token keys): the
//! conformance suite over Pub/Sub, verified HTTP callbacks, the
//! working-state message replaced by the reply, clarify cards with
//! "Other", attachment tokens kept to Google hosts, and `/setup-files`
//! giving one user native file delivery.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use common::catalog_jwt;
use pmt_gateway::messaging::conformance::{
    Failure, MockFile, MockPlatform, MockSent, kind_of_name,
};
use pmt_gateway::messaging::{Button, MediaKind, Outgoing};
use serde_json::{Value, json};

const SA_EMAIL: &str = "prometheus@agent-project.iam.gserviceaccount.com";
const USER: &str = "kit@example.com";
const DM: &str = "spaces/DM1";
const SPACE: &str = "spaces/G1";
const CLIENT: &str = "oauth-client-1";
const CHAT_SIGNER: &str = "chat@system.gserviceaccount.com";

#[derive(Default, Clone)]
struct Posted {
    name: String,
    space: String,
    thread: Option<String>,
    text: String,
    buttons: Vec<Button>,
    /// Who posted it: `bot` or the user's token.
    by: String,
    attachment: Option<String>,
}

#[derive(Default)]
struct State {
    queue: Vec<(String, Value)>,
    pulls: Vec<u64>,
    posted: Vec<Posted>,
    deleted: Vec<String>,
    media: Vec<(String, Vec<u8>)>,
    uploads: Vec<(String, String, Vec<u8>)>,
    last: Option<Value>,
    fail_next: Option<Failure>,
    ack_seq: u64,
}

struct Google {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn token_endpoint(request: &Request) -> Reply {
    if request.method == "GET" {
        return Reply::status(405);
    }
    match request.field("grant_type").as_deref() {
        Some("urn:ietf:params:oauth:grant-type:jwt-bearer") => {
            let claims = catalog_jwt::verified(&request.field("assertion").unwrap_or_default());
            let good = claims.is_some_and(|c| {
                c["iss"] == SA_EMAIL
                    && c["scope"].as_str().is_some_and(|s| s.contains("chat.bot"))
                    && c["exp"].as_u64().is_some_and(|e| e > catalog_jwt::now())
            });
            if good {
                Reply::json(
                    200,
                    &json!({"access_token": "bot-token", "expires_in": 3600}),
                )
            } else {
                Reply::json(400, &json!({"error": "invalid_grant"}))
            }
        }
        Some("refresh_token") => {
            let refresh = request.field("refresh_token").unwrap_or_default();
            let user = refresh.trim_start_matches("refresh-");
            Reply::json(
                200,
                &json!({"access_token": format!("user-token-{user}"), "expires_in": 3600}),
            )
        }
        Some("authorization_code") if request.field("code").as_deref() == Some("good-code") => {
            Reply::json(
                200,
                &json!({"access_token": "x", "refresh_token": format!("refresh-{USER}")}),
            )
        }
        _ => Reply::json(400, &json!({"error": "invalid_grant"})),
    }
}

fn who(request: &Request) -> Option<String> {
    match request.header("authorization")? {
        "Bearer bot-token" => Some("bot".to_owned()),
        other => other.strip_prefix("Bearer user-token-").map(str::to_owned),
    }
}

fn shown(body: &Value) -> (String, Vec<Button>) {
    let widgets = &body["cardsV2"][0]["card"]["sections"][0]["widgets"];
    if widgets.is_array() {
        let buttons = widgets[1]["buttonList"]["buttons"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| Button {
                label: b["text"].as_str().unwrap_or_default().to_owned(),
                data: b["onClick"]["action"]["parameters"][0]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect();
        (
            widgets[0]["textParagraph"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            buttons,
        )
    } else {
        (
            body["text"].as_str().unwrap_or_default().to_owned(),
            Vec::new(),
        )
    }
}

fn chat_api(state: &mut State, request: &Request, by: String) -> Reply {
    let route = request.route().trim_start_matches("/v1/").to_owned();
    if let Some(resource) = route.strip_prefix("media/") {
        return match state.media.iter().find(|(m, _)| m == resource) {
            Some((_, bytes)) => Reply {
                status: 200,
                headers: Vec::new(),
                body: bytes.clone(),
            },
            None => Reply::status(404),
        };
    }
    match request.method.as_str() {
        "GET" => Reply::json(
            200,
            &json!({"name": route, "spaceType": if route == DM { "DIRECT_MESSAGE" } else { "SPACE" }}),
        ),
        "DELETE" => {
            state.deleted.push(route);
            Reply::json(200, &json!({}))
        }
        "PATCH" => {
            let (text, buttons) = shown(&request.json());
            if let Some(p) = state.posted.iter_mut().find(|p| p.name == route) {
                p.text = text;
                p.buttons = buttons;
            }
            Reply::json(200, &json!({"name": route}))
        }
        _ => {
            match state.fail_next.take() {
                Some(Failure::RateLimited(_)) => return Reply::status(429),
                Some(Failure::ServerError) => return Reply::status(503),
                Some(Failure::Forbidden | Failure::Markup) => return Reply::status(403),
                None => {}
            }
            let body = request.json();
            let space = route.trim_end_matches("/messages").to_owned();
            let (text, buttons) = shown(&body);
            let name = format!("{space}/messages/out{}", state.posted.len() + 1);
            state.posted.push(Posted {
                name: name.clone(),
                space,
                thread: body["thread"]["name"].as_str().map(str::to_owned),
                text,
                buttons,
                by,
                attachment: body["attachment"][0]["attachmentDataRef"]["resourceName"]
                    .as_str()
                    .map(str::to_owned),
            });
            Reply::json(200, &json!({"name": name}))
        }
    }
}

fn pubsub(state: &mut State, request: &Request) -> Reply {
    if request.route().ends_with(":acknowledge") {
        let acks: Vec<String> = request.json()["ackIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect();
        state.queue.retain(|(ack, _)| !acks.contains(ack));
        return Reply::json(200, &json!({}));
    }
    let max = request.json()["maxMessages"].as_u64().unwrap_or(0);
    state.pulls.push(max);
    let received: Vec<Value> = state
        .queue
        .iter()
        .take(usize::try_from(max).unwrap())
        .map(|(ack, event)| json!({"ackId": ack, "message": {"data": base64_of(event)}}))
        .collect();
    Reply::json(200, &json!({"receivedMessages": received}))
}

fn base64_of(event: &Value) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(event.to_string())
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    let mut reply = if route == "/token" {
        token_endpoint(request)
    } else if route == "/certs" {
        Reply::json(200, &catalog_jwt::jwks("google-key"))
    } else if route == "/revoke" {
        Reply::json(200, &json!({}))
    } else if let Some(by) = who(request) {
        if route.starts_with("/v1/projects/") {
            pubsub(&mut state, request)
        } else if let Some(rest) = route.strip_prefix("/upload/v1/") {
            let space = rest.trim_end_matches("/attachments:upload").to_owned();
            let (name, _, bytes) = catalog::multipart_file(&request.body)
                .map_or_else(|| related_file(&request.body), Some)
                .unwrap();
            let id = format!("up{}", state.uploads.len() + 1);
            state.uploads.push((id.clone(), name, bytes));
            let _ = space;
            Reply::json(200, &json!({"attachmentDataRef": {"resourceName": id}}))
        } else {
            chat_api(&mut state, request, by)
        }
    } else {
        Reply::status(401)
    };
    reply.headers.push(catalog_jwt::date_header());
    reply
}

/// The file of a `multipart/related` upload: (name, mime, bytes).
fn related_file(body: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let text = String::from_utf8_lossy(body);
    let name = text
        .split("\"filename\":\"")
        .nth(1)?
        .split('"')
        .next()?
        .to_owned();
    let second = body
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"\r\n\r\n")
        .nth(1)?
        .0
        + 4;
    let end = body[second..].windows(4).position(|w| w == b"\r\n--")? + second;
    Some((name, String::new(), body[second..end].to_vec()))
}

impl Google {
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
        let base = self.api.base();
        json!({
            "project_id": "agent-project",
            "subscription": "chat-events",
            "chat_api": base,
            "pubsub_api": base,
            "token_url": format!("{base}/token"),
            "certs_url": format!("{base}/certs"),
            "revoke_url": format!("{base}/revoke"),
            "oauth_client_id": CLIENT,
        })
    }

    fn service_account(&self) -> String {
        json!({
            "type": "service_account",
            "client_email": SA_EMAIL,
            "private_key_id": "k1",
            "private_key": catalog_jwt::PEM,
            "token_uri": format!("{}/token", self.api.base()),
        })
        .to_string()
    }

    fn enqueue(&self, event: Value) {
        let mut state = self.state.lock().unwrap();
        state.ack_seq += 1;
        let ack = format!("ack{}", state.ack_seq);
        state.last = Some(event.clone());
        state.queue.push((ack, event));
    }

    fn message_event(&self, space: &str, text: &str, extra: &Value) -> (String, Value) {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{space}/messages/in{n}");
        let space_type = if space == DM {
            "DIRECT_MESSAGE"
        } else {
            "SPACE"
        };
        let mut message = json!({
            "name": name,
            "sender": {"name": "users/1", "email": USER, "displayName": "Kit User", "type": "HUMAN"},
            "text": text,
            "argumentText": text,
            "thread": {"name": format!("{space}/threads/t{n}")},
        });
        if let (Some(m), Some(e)) = (message.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        let event = json!({"type": "MESSAGE", "space": {"name": space, "spaceType": space_type}, "message": message});
        (name, event)
    }

    fn say(&self, space: &str, text: &str) -> String {
        let (name, event) = self.message_event(space, text, &json!({}));
        self.enqueue(event);
        name
    }

    fn posted(&self) -> Vec<Posted> {
        self.state.lock().unwrap().posted.clone()
    }
}

impl MockPlatform for Google {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.say(DM, text)
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(event) = last {
            self.enqueue(event);
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, message_id: &str, data: &str) {
        let space = message_id.split("/messages/").next().unwrap_or(DM);
        self.enqueue(json!({
            "type": "CARD_CLICKED",
            "space": {"name": space, "spaceType": "DIRECT_MESSAGE"},
            "message": {"name": message_id},
            "user": {"name": "users/1", "email": USER},
            "common": {"invokedFunction": "prometheus", "parameters": {"data": data}},
        }));
    }

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
        self.posted()
            .into_iter()
            .map(|p| MockSent {
                chat_id: p.space,
                message_id: p.name,
                text: p.text,
                buttons: p.buttons,
                thread_id: p.thread,
            })
            .collect()
    }

    fn typing(&self) -> usize {
        self.posted()
            .iter()
            .filter(|p| p.by == "bot" && p.text == "Prometheus is thinking…")
            .count()
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn thread(&self) -> Option<(String, String)> {
        Some((SPACE.to_owned(), format!("{SPACE}/threads/kit")))
    }

    fn deleted(&self) -> Vec<String> {
        self.state.lock().unwrap().deleted.clone()
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let resource = format!("media{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .media
            .push((resource.clone(), bytes.to_vec()));
        let mime = match kind {
            MediaKind::Image => "image/png",
            MediaKind::Audio | MediaKind::Voice => "audio/mpeg",
            MediaKind::Video => "video/mp4",
            _ => "application/pdf",
        };
        let attachment = json!([{"contentName": name, "contentType": mime, "source": "UPLOADED_CONTENT", "attachmentDataRef": {"resourceName": resource}}]);
        let (id, event) = self.message_event(DM, caption, &json!({"attachment": attachment}));
        self.enqueue(event);
        Some(id)
    }

    fn files(&self) -> Vec<MockFile> {
        let state = self.state.lock().unwrap();
        state
            .posted
            .iter()
            .filter_map(|p| {
                let (_, name, bytes) = state
                    .uploads
                    .iter()
                    .find(|u| Some(&u.0) == p.attachment.as_ref())?;
                Some(MockFile {
                    chat_id: p.space.clone(),
                    thread_id: p.thread.clone(),
                    name: name.clone(),
                    kind: kind_of_name(name),
                    bytes: bytes.clone(),
                })
            })
            .collect()
    }

    fn voice_notes_out(&self) -> bool {
        false
    }
}

fn install(
    mock: &Google,
    options: &Value,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("google_chat", options, &[mock.api.host()]);
    catalog::secret(
        &kit,
        "GOOGLE_CHAT_SERVICE_ACCOUNT_JSON",
        &mock.service_account(),
    );
    catalog::secret(&kit, "GOOGLE_CHAT_OAUTH_CLIENT_SECRET", "oauth-secret");
    (kit, root)
}

async fn connected(
    mock: &Arc<Google>,
    kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit,
) -> (Arc<dyn pmt_gateway::messaging::Adapter>, Arc<catalog::Link>) {
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let _ = mock;
    (adapter, link)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_google_chat_adapter_passes_the_conformance_suite() {
    let mock = Google::start();
    let (kit, _root) = install(&mock, &mock.options());
    catalog::secret(
        &kit,
        "GOOGLE_CHAT_USER_TOKENS",
        &json!({USER: format!("refresh-{USER}")}).to_string(),
    );
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
    // One message per pull unless max_messages says otherwise.
    assert!(mock.state.lock().unwrap().pulls.iter().all(|m| *m == 1));
}

/// Scenario: Per-user file delivery.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_who_completed_setup_files_gets_native_files() {
    let mock = Google::start();
    let (kit, _root) = install(&mock, &mock.options());
    let (adapter, link) = connected(&mock, &kit).await;
    let pdf = {
        let dir = catalog::scratch();
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, b"%PDF-1.7 report").unwrap();
        (dir, path)
    };
    let mut file_reply = Outgoing::text(DM, None, "");
    file_reply
        .attachments
        .push(pmt_store::gateway::outbox::OutboundAttachment::new(
            &pdf.1.to_string_lossy(),
        ));
    // Before /setup-files: a notice, no upload.
    mock.say(DM, "hello");
    link.wait_for(1).await;
    adapter.send(&file_reply).await.unwrap();
    assert!(mock.state.lock().unwrap().uploads.is_empty());
    let notice = mock.posted().pop().unwrap();
    assert!(
        notice.text.contains("/setup-files start"),
        "{}",
        notice.text
    );
    // The flow: the link, then the pasted redirect.
    mock.say(DM, "/setup-files start");
    let link_text = loop_until(&mock, |p| p.text.contains("access_type=offline")).await;
    let state = link_text
        .split("state=")
        .nth(1)
        .unwrap()
        .split(['&', ' '])
        .next()
        .unwrap()
        .to_owned();
    assert!(link_text.contains(&format!("client_id={CLIENT}")));
    assert!(link_text.contains("chat.messages.create"));
    mock.say(
        DM,
        &format!("/setup-files http://localhost:8085/?state={state}&code=good-code"),
    );
    loop_until(&mock, |p| p.text.starts_with("Done")).await;
    // The token reached the secrets store; the setup messages reached no turn.
    let stored = wait_secret(&kit).await;
    assert_eq!(stored[USER], format!("refresh-{USER}"));
    assert_eq!(link.wait_for(2).await.len(), 1);
    // Now the PDF goes up natively, posted with the user's own token.
    adapter.send(&file_reply).await.unwrap();
    let uploads = mock.state.lock().unwrap().uploads.clone();
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert_eq!(uploads[0].2, b"%PDF-1.7 report");
    let posted = mock.posted().pop().unwrap();
    assert_eq!(posted.by, USER);
    assert_eq!(posted.attachment.as_deref(), Some(uploads[0].0.as_str()));
    // Revoking removes it again.
    mock.say(DM, "/setup-files revoke");
    loop_until(&mock, |p| p.text.starts_with("Revoked")).await;
    adapter.disconnect().await;
}

async fn loop_until(mock: &Google, mut done: impl FnMut(&Posted) -> bool) -> String {
    for _ in 0..200 {
        if let Some(p) = mock.posted().into_iter().find(|p| done(p)) {
            return p.text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "never posted: {:#?}",
        mock.posted().iter().map(|p| &p.text).collect::<Vec<_>>()
    );
}

async fn wait_secret(kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit) -> Value {
    for _ in 0..100 {
        if let Ok(value) = kit
            .setup()
            .scope
            .secrets()
            .resolve("GOOGLE_CHAT_USER_TOKENS")
        {
            return serde_json::from_str(value.expose()).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the user tokens were never stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_callbacks_need_a_google_id_token_for_this_app() {
    let mock = Google::start();
    let mut options = mock.options();
    options["mode"] = json!("http");
    options["audience"] = json!("https://agent.example/p/default/google_chat/");
    options["service_account_email"] = json!(CHAT_SIGNER);
    let (kit, _root) = install(&mock, &options);
    let adapter = kit.adapter().unwrap();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let (_, event) = mock.message_event(DM, "over http", &json!({}));
    let post = |token: Option<String>| {
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut headers = vec![("content-type", "application/json")];
        if let Some(a) = &auth {
            headers.push(("authorization", a));
        }
        catalog::post(
            &format!("{prefix}/"),
            &headers,
            event.to_string().as_bytes(),
        )
        .0
    };
    let claims = |aud: &str, email: &str| json!({"iss": "https://accounts.google.com", "aud": aud, "email": email, "exp": catalog_jwt::now() + 600, "iat": catalog_jwt::now()});
    let audience = "https://agent.example/p/default/google_chat/";
    assert_eq!(post(None), 401);
    assert_eq!(
        post(Some(catalog_jwt::token(
            "google-key",
            &claims("https://other", CHAT_SIGNER)
        ))),
        401
    );
    assert_eq!(
        post(Some(catalog_jwt::token(
            "google-key",
            &claims(audience, "evil@example.com")
        ))),
        401
    );
    assert!(link.wait_for(1).await.is_empty());
    assert_eq!(
        post(Some(catalog_jwt::token(
            "google-key",
            &claims(audience, CHAT_SIGNER)
        ))),
        200
    );
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].sender_id.as_str(), got[0].text.as_str()),
        (USER, "over http")
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_working_message_becomes_the_reply_and_clarify_offers_other() {
    let mock = Google::start();
    let (kit, _root) = install(&mock, &mock.options());
    let (adapter, link) = connected(&mock, &kit).await;
    mock.say(SPACE, "@Prometheus status?");
    let got = link.wait_for(1).await;
    let thread = got[0].thread_id.clone();
    assert!(thread.is_some(), "a space keeps one session per thread");
    adapter.send_typing(SPACE, thread.as_deref()).await.unwrap();
    adapter.send_typing(SPACE, thread.as_deref()).await.unwrap();
    let thinking: Vec<Posted> = mock.posted();
    assert_eq!(thinking.len(), 1);
    assert_eq!(thinking[0].text, "Prometheus is thinking…");
    let reply = adapter
        .send(&Outgoing::text(SPACE, thread.as_deref(), "All green."))
        .await
        .unwrap();
    assert_eq!(reply.message_id.as_deref(), Some(thinking[0].name.as_str()));
    let posted = mock.posted();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].text, "All green.");
    // A clarify card: one button per choice, plus Other.
    let mut card = Outgoing::text(SPACE, thread.as_deref(), "Which region?");
    card.buttons = ["eu", "us"]
        .iter()
        .enumerate()
        .map(|(i, c)| Button {
            label: (*c).to_owned(),
            data: format!("clar:q1:0.{}:0123456789abcdef", i + 1),
        })
        .collect();
    let id = adapter.send(&card).await.unwrap().message_id.unwrap();
    let shown = mock.posted().into_iter().find(|p| p.name == id).unwrap();
    let labels: Vec<&str> = shown.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["eu", "us", "Other"]);
    mock.press(&id, &shown.buttons[2].data);
    loop_until(&mock, |p| p.text == "Type your answer as a message.").await;
    assert_eq!(
        link.wait_for(2).await.len(),
        1,
        "Other is no answer by itself"
    );
    adapter.disconnect().await;

    // Without the indicator, typing posts nothing.
    let mut options = mock.options();
    options["typing_indicator"] = json!(false);
    let (kit, _root) = install(&mock, &options);
    let (adapter, _link) = connected(&mock, &kit).await;
    let before = mock.posted().len();
    adapter.send_typing(DM, None).await.unwrap();
    assert_eq!(mock.posted().len(), before);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_tokens_go_only_to_google_hosts() {
    let mock = Google::start();
    let elsewhere = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let seen = Arc::clone(&elsewhere);
    let other = MockHttp::start(Arc::new(move |request: &Request| {
        seen.lock()
            .unwrap()
            .push(request.header("authorization").map(str::to_owned));
        Reply {
            status: 200,
            headers: Vec::new(),
            body: b"stolen?".to_vec(),
        }
    }));
    let (kit, _root) = catalog::install(
        "google_chat",
        &mock.options(),
        &[mock.api.host(), other.host()],
    );
    catalog::secret(
        &kit,
        "GOOGLE_CHAT_SERVICE_ACCOUNT_JSON",
        &mock.service_account(),
    );
    let (adapter, link) = connected(&mock, &kit).await;
    let attachment = json!([{"contentName": "x.png", "contentType": "image/png", "downloadUri": format!("{}/x.png", other.base())}]);
    let (_, event) = mock.message_event(DM, "look", &json!({"attachment": attachment}));
    mock.enqueue(event);
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1);
    assert!(got[0].attachments.is_empty());
    assert!(elsewhere.lock().unwrap().is_empty(), "no request, no token");
    adapter.disconnect().await;
}
