//! The `sms` catalog adapter plugin (spec: platform-adapters, SMS adapter;
//! Catalog adapter set) against a mock of Twilio: the conformance suite,
//! signed webhooks in (a bad `X-Twilio-Signature` is 401 and admits
//! nothing), plain-text replies out, the adapter's own number ignored.

#![cfg(feature = "gateway")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::catalog::{self, MockHttp, Reply, Request};
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

const TOKEN: &str = "twilio-auth-token";
const SID: &str = "AC1";
const OWN: &str = "+15550009999";
const USER: &str = "+15550001111";
/// The webhook URL configured in Twilio (what it signs).
const PUBLIC_URL: &str = "https://sms.example.com/p/default/sms/";

/// Twilio's signature of a form POST.
fn twilio_signature(url: &str, params: &[(&str, &str)]) -> String {
    let mut sorted = params.to_vec();
    sorted.sort_unstable();
    let mut data = url.to_owned();
    for (k, v) in sorted {
        data.push_str(k);
        data.push_str(v);
    }
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, TOKEN.as_bytes());
    base64::engine::general_purpose::STANDARD
        .encode(ring::hmac::sign(&key, data.as_bytes()).as_ref())
}

#[derive(Default)]
struct State {
    adapter: Option<String>,
    sent: Vec<MockSent>,
    fail_next: Option<Failure>,
    last: Option<(String, String)>,
}

/// Twilio: the Messages API and the webhooks it posts.
struct Twilio {
    api: MockHttp,
    state: Arc<Mutex<State>>,
    next: AtomicU64,
}

impl Twilio {
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
            "account_sid": SID,
            "phone_number": OWN,
            "public_url": PUBLIC_URL,
            "api_base": self.api.base(),
        })
    }

    /// Posts an inbound SMS webhook signed with `signature` (the right one
    /// when `None`): the adapter's status.
    fn deliver(&self, sid: &str, from: &str, body: &str, signature: Option<&str>) -> u16 {
        let params = [
            ("MessageSid", sid),
            ("From", from),
            ("To", OWN),
            ("Body", body),
        ];
        let right = twilio_signature(PUBLIC_URL, &params);
        let form: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        let base = self
            .state
            .lock()
            .unwrap()
            .adapter
            .clone()
            .expect("the kit listens");
        catalog::post(
            &format!("{base}/"),
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("x-twilio-signature", signature.unwrap_or(&right)),
            ],
            form.as_bytes(),
        )
        .0
    }
}

fn answer(state: &Mutex<State>, request: &Request) -> Reply {
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{SID}:{TOKEN}"))
    );
    if request.header("authorization") != Some(expected.as_str()) {
        return Reply::json(401, &json!({"code": 20003}));
    }
    if request.method != "POST"
        || request.route() != format!("/2010-04-01/Accounts/{SID}/Messages.json")
    {
        return Reply::status(404);
    }
    let mut state = state.lock().unwrap();
    match state.fail_next.take() {
        Some(Failure::RateLimited(_)) => return Reply::json(429, &json!({"code": 20429})),
        Some(Failure::ServerError) => return Reply::status(500),
        Some(Failure::Forbidden | Failure::Markup) => {
            return Reply::json(403, &json!({"code": 21610}));
        }
        None => {}
    }
    let body = request.field("Body").unwrap_or_default();
    if body.chars().count() > 1600 {
        return Reply::json(400, &json!({"code": 21617, "message": "body too long"}));
    }
    let id = format!("SM{}", state.sent.len() + 1);
    state.sent.push(MockSent {
        chat_id: request.field("To").unwrap_or_default(),
        message_id: id.clone(),
        text: body,
        buttons: Vec::new(),
        thread_id: None,
    });
    Reply::json(201, &json!({"sid": id}))
}

impl MockPlatform for Twilio {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        let sid = format!("SMin{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.state.lock().unwrap().last = Some((sid.clone(), text.to_owned()));
        assert_eq!(self.deliver(&sid, USER, text, None), 200);
        sid
    }

    fn redeliver(&self) {
        let last = self.state.lock().unwrap().last.clone();
        if let Some((sid, text)) = last {
            self.deliver(&sid, USER, &text, None);
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

    fn sent(&self) -> Vec<MockSent> {
        self.state.lock().unwrap().sent.clone()
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

    fn hints_retry_after(&self) -> bool {
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sms_adapter_passes_the_conformance_suite() {
    let twilio = Twilio::start();
    let report = catalog::conformance(
        "sms",
        &twilio.options(),
        &[twilio.api.host()],
        &[("TWILIO_AUTH_TOKEN", TOKEN)],
        Arc::clone(&twilio) as Arc<dyn MockPlatform>,
    )
    .await;
    catalog::assert_passed(&report);
}

/// Scenario: Bad Twilio signature.
#[tokio::test(flavor = "multi_thread")]
async fn a_badly_signed_sms_is_refused_and_admits_nothing() {
    let twilio = Twilio::start();
    let (kit, _root) = catalog::install("sms", &twilio.options(), &[twilio.api.host()]);
    catalog::secret(&kit, "TWILIO_AUTH_TOKEN", TOKEN);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    twilio.listen(&prefix);
    adapter.connect(link.dyn_link()).await.unwrap();
    let forged = twilio.deliver("SMbad", USER, "hello", Some("Zm9yZ2Vk"));
    assert_eq!(forged, 401);
    // Its own number is ignored; a good one is admitted with Markdown-free
    // replies.
    assert_eq!(twilio.deliver("SMown", OWN, "echo", None), 200);
    assert_eq!(twilio.deliver("SMok", USER, "hi there", None), 200);
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].text, "hi there");
    assert_eq!(admitted[0].sender_id, USER);
    let reply =
        pmt_gateway::messaging::Outgoing::text(USER, None, "**Done**: see [docs](https://x.y)");
    adapter.send(&reply).await.unwrap();
    assert_eq!(twilio.sent()[0].text, "Done: see docs (https://x.y)");
    adapter.disconnect().await;
}
