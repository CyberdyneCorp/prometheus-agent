//! The `teams` catalog adapter plugin (spec: platform-adapters, Microsoft
//! Teams adapter; Catalog adapter set) against a mock Bot Framework (its
//! OpenID metadata and keys, the Entra token endpoint and a Bot
//! Connector): the conformance suite, JWT verification before admission,
//! mention gating in groups and channels, approval cards replaced in
//! place, and the body cap.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use common::catalog_jwt;
use pmt_gateway::messaging::conformance::{Failure, MockFile, MockPlatform, MockSent};
use pmt_gateway::messaging::{Button, MediaKind, Outgoing};
use serde_json::{Value, json};

const APP_ID: &str = "00000000-0000-0000-0000-00000000b07a";
const TENANT: &str = "tenant-1";
const SECRET: &str = "teams-client-secret";
const DM: &str = "a:kit-dm";
const GROUP: &str = "19:group-1@thread.v2";
const CHANNEL: &str = "19:channel-1@thread.tacv2";
const USER: &str = "aad-user-1";
const KID: &str = "bf-key";

#[derive(Default)]
struct State {
    adapter: Option<String>,
    /// Activities the bot posted: (id, conversation, activity).
    posted: Vec<(String, String, Value)>,
    deleted: Vec<String>,
    typing: usize,
    files: Vec<(String, Vec<u8>)>,
    last: Option<Value>,
    fail_next: Option<Failure>,
    tokens_issued: usize,
}

struct BotFramework {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

fn connector(state: &mut State, request: &Request, route: &str) -> Reply {
    if request.header("authorization") != Some("Bearer bot-token") {
        return Reply::status(401);
    }
    let rest = route.strip_prefix("/v3/conversations/").unwrap_or_default();
    let (conversation, activity_id) = match rest.split_once("/activities") {
        Some((c, tail)) => (
            url::form_urlencoded::parse(format!("c={c}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default(),
            tail.trim_start_matches('/').to_owned(),
        ),
        None => return Reply::status(404),
    };
    match request.method.as_str() {
        "DELETE" => {
            state.deleted.push(activity_id);
            Reply::status(200)
        }
        "PUT" => {
            if let Some(entry) = state.posted.iter_mut().find(|(id, ..)| *id == activity_id) {
                entry.2 = request.json();
            }
            Reply::json(200, &json!({"id": activity_id}))
        }
        _ => {
            let activity = request.json();
            if activity["type"] == "typing" {
                state.typing += 1;
                return Reply::json(200, &json!({}));
            }
            match state.fail_next.take() {
                Some(Failure::RateLimited(secs)) => {
                    let mut reply = Reply::status(429);
                    reply.headers.push(("retry-after".into(), secs.to_string()));
                    return reply;
                }
                Some(Failure::ServerError) => return Reply::status(502),
                Some(Failure::Forbidden | Failure::Markup) => return Reply::status(403),
                None => {}
            }
            let id = format!("act-{}", state.posted.len() + 1);
            state.posted.push((id.clone(), conversation, activity));
            Reply::json(201, &json!({"id": id}))
        }
    }
}

fn answer(base: &str, state: &Mutex<State>, request: &Request) -> Reply {
    let mut state = state.lock().unwrap();
    let route = request.route().to_owned();
    let mut reply = match route.as_str() {
        "/openid" => Reply::json(200, &json!({"jwks_uri": format!("{base}/keys")})),
        "/keys" => Reply::json(200, &catalog_jwt::jwks(KID)),
        r if r == format!("/{TENANT}/oauth2/v2.0/token") => {
            let ok = request.field("client_secret").as_deref() == Some(SECRET)
                && request.field("scope").as_deref()
                    == Some("https://api.botframework.com/.default");
            if !ok {
                return Reply::status(401);
            }
            state.tokens_issued += 1;
            Reply::json(
                200,
                &json!({"access_token": "bot-token", "expires_in": 3600}),
            )
        }
        r if r.starts_with("/files/") => {
            let id = r.trim_start_matches("/files/");
            match state.files.iter().find(|(f, _)| f == id) {
                Some((_, bytes)) => Reply {
                    status: 200,
                    headers: Vec::new(),
                    body: bytes.clone(),
                },
                None => Reply::status(404),
            }
        }
        r if r.starts_with("/v3/conversations/") => connector(&mut state, request, r),
        _ => Reply::status(404),
    };
    reply.headers.push(catalog_jwt::date_header());
    reply
}

impl BotFramework {
    fn start() -> Arc<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let base = Arc::new(Mutex::new(String::new()));
        let known = Arc::clone(&base);
        let api = MockHttp::start(Arc::new(move |request: &Request| {
            let base = known.lock().unwrap().clone();
            answer(&base, &served, request)
        }));
        api.base().clone_into(&mut base.lock().unwrap());
        Arc::new(Self {
            api,
            state,
            next: AtomicU64::new(1),
        })
    }

    fn options(&self) -> Value {
        json!({
            "client_id": APP_ID,
            "tenant_id": TENANT,
            "openid_metadata_url": format!("{}/openid", self.api.base()),
            "login_base": self.api.base(),
            "service_url": self.api.base(),
        })
    }

    fn claims(&self) -> Value {
        let now = catalog_jwt::now();
        json!({
            "iss": "https://api.botframework.com",
            "aud": APP_ID,
            "serviceurl": self.api.base(),
            "nbf": now - 60,
            "exp": now + 3600,
        })
    }

    fn post_with(&self, activity: &Value, token: Option<String>) -> u16 {
        let prefix = self
            .state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens");
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut headers = vec![("content-type", "application/json")];
        if let Some(auth) = &auth {
            headers.push(("authorization", auth));
        }
        catalog::post(
            &format!("{prefix}/api/messages"),
            &headers,
            activity.to_string().as_bytes(),
        )
        .0
    }

    /// Status of a POST declaring a `length`-byte body (sending only its
    /// first bytes): the cap answers from the declared length.
    fn declare_body(&self, length: usize) -> u16 {
        use std::io::{Read as _, Write as _};
        let prefix = self
            .state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens");
        let url = url::Url::parse(&prefix).unwrap();
        let mut stream =
            std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
        let head = format!(
            "POST {}/api/messages HTTP/1.1\r\nhost: x\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{{}}",
            url.path()
        );
        stream.write_all(head.as_bytes()).unwrap();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn post(&self, activity: &Value) -> u16 {
        let token = catalog_jwt::token(KID, &self.claims());
        self.post_with(activity, Some(token))
    }

    fn activity(&self, conversation: &str, kind: &str, text: &str) -> Value {
        let id = format!("in-{}", self.next.fetch_add(1, Ordering::Relaxed));
        json!({
            "type": "message",
            "id": id,
            "serviceUrl": self.api.base(),
            "channelId": "msteams",
            "from": {"id": "29:user-1", "aadObjectId": USER, "name": "Kit User"},
            "recipient": {"id": format!("28:{APP_ID}"), "name": "Prometheus"},
            "conversation": {"id": conversation, "conversationType": kind},
            "text": text,
        })
    }

    fn deliver(&self, activity: &Value) -> String {
        let id = activity["id"].as_str().unwrap().to_owned();
        self.state.lock().unwrap().last = Some(activity.clone());
        assert_eq!(self.post(activity), 200);
        id
    }

    fn posted(&self) -> Vec<(String, String, Value)> {
        self.state.lock().unwrap().posted.clone()
    }
}

fn shown(id: &str, conversation: &str, activity: &Value) -> MockSent {
    let card = &activity["attachments"][0]["content"];
    let (text, buttons) = if card.is_object() {
        let buttons = card["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| Button {
                label: a["title"].as_str().unwrap_or_default().to_owned(),
                data: a["data"]["prometheus"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect();
        (
            card["fallbackText"].as_str().unwrap_or_default().to_owned(),
            buttons,
        )
    } else {
        (
            activity["text"].as_str().unwrap_or_default().to_owned(),
            Vec::new(),
        )
    };
    MockSent {
        chat_id: conversation.to_owned(),
        message_id: id.to_owned(),
        text,
        buttons,
        thread_id: None,
    }
}

impl MockPlatform for BotFramework {
    fn dm(&self) -> (String, String) {
        (DM.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.deliver(&self.activity(DM, "personal", text))
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some(activity) = last {
            self.post(&activity);
        }
    }

    fn drop_connection(&self) {}

    fn press(&self, message_id: &str, data: &str) {
        let mut invoke = self.activity(DM, "personal", "");
        invoke["type"] = json!("invoke");
        invoke["name"] = json!("adaptiveCard/action");
        invoke["replyToId"] = json!(message_id);
        invoke["value"] = json!({"action": {"type": "Action.Execute", "verb": "prometheus", "data": {"prometheus": data}}});
        assert_eq!(self.post(&invoke), 200);
    }

    fn fail_next(&self, failure: Failure) {
        self.state.lock().unwrap().fail_next = Some(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.posted()
            .iter()
            .map(|(id, conversation, activity)| shown(id, conversation, activity))
            .collect()
    }

    fn typing(&self) -> usize {
        self.state.lock().unwrap().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn deleted(&self) -> Vec<String> {
        self.state.lock().unwrap().deleted.clone()
    }

    fn listen(&self, base: &str) {
        self.state.lock().unwrap().adapter = Some(base.to_owned());
    }

    fn inject_media(
        &self,
        kind: MediaKind,
        name: &str,
        bytes: &[u8],
        caption: &str,
    ) -> Option<String> {
        let file = format!("f{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state
            .lock()
            .unwrap()
            .files
            .push((file.clone(), bytes.to_vec()));
        let url = format!("{}/files/{file}", self.api.base());
        let attachment = match kind {
            MediaKind::Image => {
                json!({"contentType": "image/png", "contentUrl": url, "name": name})
            }
            _ => json!({
                "contentType": "application/vnd.microsoft.teams.file.download.info",
                "name": name,
                "content": {"downloadUrl": url, "fileType": "pdf"},
            }),
        };
        let mut activity = self.activity(DM, "personal", caption);
        activity["attachments"] = json!([attachment]);
        Some(self.deliver(&activity))
    }

    fn files(&self) -> Vec<MockFile> {
        Vec::new()
    }
}

fn install(
    mock: &BotFramework,
) -> (
    prometheus::plugins_cmd::adapter_plugin::PluginKit,
    tempfile::TempDir,
) {
    let (kit, root) = catalog::install("teams", &mock.options(), &[mock.api.host()]);
    catalog::secret(&kit, "TEAMS_CLIENT_SECRET", SECRET);
    (kit, root)
}

async fn connected(
    mock: &Arc<BotFramework>,
) -> (
    Arc<dyn pmt_gateway::messaging::Adapter>,
    Arc<catalog::Link>,
    tempfile::TempDir,
) {
    let (kit, root) = install(mock);
    let adapter = kit.adapter().unwrap();
    mock.listen(&catalog::serve(Arc::clone(&adapter)).await);
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    (adapter, link, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_teams_adapter_passes_the_conformance_suite() {
    let mock = BotFramework::start();
    let (kit, _root) = install(&mock);
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
}

/// Scenario: Unsigned activity.
#[tokio::test(flavor = "multi_thread")]
async fn an_activity_without_a_valid_jwt_is_refused_and_admits_nothing() {
    let mock = BotFramework::start();
    let (adapter, link, _root) = connected(&mock).await;
    let activity = mock.activity(DM, "personal", "sneaky");
    assert_eq!(mock.post_with(&activity, None), 401);
    let mut claims = mock.claims();
    claims["aud"] = json!("someone-else");
    let other_audience = catalog_jwt::token(KID, &claims);
    assert_eq!(mock.post_with(&activity, Some(other_audience)), 401);
    let mut claims = mock.claims();
    claims["exp"] = json!(catalog_jwt::now() - 3600);
    let expired = catalog_jwt::token(KID, &claims);
    assert_eq!(mock.post_with(&activity, Some(expired)), 401);
    let token = catalog_jwt::token(KID, &mock.claims());
    let forged = format!("{}x", &token[..token.len() - 1]);
    assert_eq!(mock.post_with(&activity, Some(forged)), 401);
    let mut claims = mock.claims();
    claims["serviceurl"] = json!("https://evil.example");
    let elsewhere = catalog_jwt::token(KID, &claims);
    assert_eq!(mock.post_with(&activity, Some(elsewhere)), 401);
    assert!(link.wait_for(1).await.is_empty());
    // A body over 1 MiB is refused before it is read.
    assert_eq!(mock.declare_body(1_100_000), 413);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn groups_and_channels_need_a_mention_or_a_reply_to_the_bot() {
    let mock = BotFramework::start();
    let (adapter, link, _root) = connected(&mock).await;
    mock.deliver(&mock.activity(GROUP, "groupChat", "chatter"));
    mock.deliver(&mock.activity(CHANNEL, "channel", "rsc chatter"));
    let mut mention = mock.activity(GROUP, "groupChat", "<at>Prometheus</at> status please");
    mention["entities"] = json!([{"type": "mention", "mentioned": {"id": format!("28:{APP_ID}"), "name": "Prometheus"}, "text": "<at>Prometheus</at>"}]);
    mock.deliver(&mention);
    let got = link.wait_for(1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].text, "status please");
    assert_eq!(got[0].sender_id, USER);
    // A reply to the bot's message counts as addressing it.
    let sent = adapter
        .send(&Outgoing::text(CHANNEL, None, "posted in the channel"))
        .await
        .unwrap();
    let mut reply = mock.activity(CHANNEL, "channel", "thanks");
    reply["replyToId"] = json!(sent.message_id.unwrap());
    mock.deliver(&reply);
    let got = link.wait_for(2).await;
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1].text, "thanks");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_card_is_replaced_in_place_with_the_decision() {
    let mock = BotFramework::start();
    let (adapter, link, _root) = connected(&mock).await;
    mock.inject("run it");
    link.wait_for(1).await;
    let mut card = Outgoing::text(DM, None, "Approval needed: terminal `ls`");
    card.buttons = ["once", "session", "always", "deny"]
        .iter()
        .map(|c| Button {
            label: (*c).to_owned(),
            data: format!("appr:srq-1:{c}:0123456789abcdef"),
        })
        .collect();
    let id = adapter.send(&card).await.unwrap().message_id.unwrap();
    let shown_card = mock
        .sent()
        .into_iter()
        .find(|m| m.message_id == id)
        .unwrap();
    let labels: Vec<&str> = shown_card
        .buttons
        .iter()
        .map(|b| b.label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["Allow Once", "Allow Session", "Always Allow", "Deny"]
    );
    mock.press(&id, &shown_card.buttons[1].data);
    let got = link.wait_for(2).await;
    assert_eq!(
        got[1].callback.as_deref(),
        Some("appr:srq-1:session:0123456789abcdef")
    );
    // The gateway replaces the card with the outcome.
    adapter
        .edit(&id, &Outgoing::text(DM, None, "Approved for this session"))
        .await
        .unwrap();
    let replaced = mock
        .posted()
        .into_iter()
        .find(|(i, ..)| *i == id)
        .unwrap()
        .2;
    let content = &replaced["attachments"][0]["content"];
    assert_eq!(content["body"][0]["text"], "Approved for this session");
    assert_eq!(content["actions"], json!([]));
    adapter.disconnect().await;
}
