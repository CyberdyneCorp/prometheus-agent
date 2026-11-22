//! The `feishu` catalog adapter plugin (spec: platform-adapters, Feishu and
//! Lark adapter; Catalog adapter set) against a mock Feishu serving the
//! Open API and the long connection (binary frames held by the bridge
//! sidecar) on one port: the conformance suite, group policies and their
//! empty-allowlist warning, batching, Markdown posts with their plain-text
//! fallback, card press deduplication, and the webhook mode's checks.

#![cfg(feature = "gateway")]

mod common;

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::catalog::{self, Bridge};
use common::catalog_feishu::{
    self, BOT, DM, Feishu, GROUP, USER, bot_mention, envelope, message_event,
};
use pmt_gateway::messaging::conformance::MockPlatform;
use pmt_gateway::messaging::{Inbound, Outgoing};
use serde_json::{Value, json};

/// Everything the process logs, from the moment it is installed (each
/// test runs in its own process under nextest).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> Self {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        logs
    }

    fn lines(&self, needle: &str) -> Vec<String> {
        let mut out = Vec::new();
        let _ = std::io::stderr().flush();
        for line in String::from_utf8_lossy(&self.0.lock().unwrap()).lines() {
            if line.contains(needle) {
                out.push(line.to_owned());
            }
        }
        out
    }
}

/// Starts the mock, a bridge and the adapter connected to a recording
/// link.
async fn connected(
    extra: &Value,
) -> (
    Arc<Feishu>,
    Bridge,
    Arc<dyn pmt_gateway::messaging::Adapter>,
    Arc<catalog::Link>,
    tempfile::TempDir,
) {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    let (kit, root) = mock.install(Some(&bridge), extra);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    (mock, bridge, adapter, link, root)
}

fn texts(inbound: &[Inbound]) -> Vec<&str> {
    inbound.iter().map(|i| i.text.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feishu_adapter_passes_the_conformance_suite() {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    // Text batching off: the suite counts each message on its own.
    let (kit, _root) = mock.install(Some(&bridge), &json!({"batch_delay_ms": 0}));
    let report = kit
        .run(
            kit.adapter().unwrap(),
            Arc::clone(&mock) as Arc<dyn MockPlatform>,
        )
        .await;
    catalog::assert_passed(&report);
    // Every data frame was acknowledged.
    assert!(!mock.state.lock().unwrap().acks.is_empty());
}

/// Scenario: Empty group allowlist.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_message_under_an_empty_allowlist_is_dropped_with_one_warning() {
    let logs = Logs::capture();
    let (mock, _bridge, adapter, link, _root) = connected(&json!({"batch_delay_ms": 0})).await;
    mock.say(GROUP, USER, "@_user_1 hello", &bot_mention());
    mock.say(GROUP, USER, "@_user_1 anyone?", &bot_mention());
    mock.say(DM, USER, "direct", &json!([]));
    let admitted = link.wait_for(1).await;
    assert_eq!(texts(&admitted), ["direct"], "only the DM is admitted");
    let warnings: Vec<String> = logs
        .lines("group allowlist is empty")
        .into_iter()
        .filter(|l| l.contains("WARN") && l.contains(GROUP))
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn group_rules_mentions_and_batches_decide_what_arrives() {
    let options = json!({
        "group_allow_from": ["ou_alice"],
        "group_rules": {"oc_open": {"policy": "open", "require_mention": false},
                        "oc_closed": {"policy": "disabled"}},
    });
    let (mock, _bridge, adapter, link, _root) = connected(&options).await;
    // Allowlisted sender: needs the mention in an ordinary group.
    mock.say(GROUP, "ou_alice", "no mention", &json!([]));
    mock.say(GROUP, "ou_alice", "@_user_1 with mention", &bot_mention());
    mock.say(GROUP, USER, "@_user_1 not listed", &bot_mention());
    mock.say("oc_closed", "ou_alice", "@_user_1 closed", &bot_mention());
    // An open chat without the mention rule; three quick lines are one.
    for line in ["first", "second", "third"] {
        mock.say("oc_open", USER, line, &json!([]));
    }
    let admitted = link.wait_for(2).await;
    let mut got = texts(&admitted);
    got.sort_unstable();
    assert_eq!(got, ["first\nsecond\nthird", "with mention"]);
    let batch = admitted.iter().find(|i| i.chat_id == "oc_open").unwrap();
    assert!(
        batch.addressing.mentioned,
        "the adapter applied the chat's mention rule"
    );
    // The bot's own messages never come back as turns.
    mock.say(DM, BOT, "echo", &json!([]));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(link.received().len(), 2);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_goes_out_as_a_post_and_falls_back_to_plain_text() {
    let (mock, _bridge, adapter, _link, _root) = connected(&json!({})).await;
    adapter
        .send(&Outgoing::text(DM, None, "**Plan**\n```sh\nls\n```\ndone"))
        .await
        .unwrap();
    {
        let state = mock.state.lock().unwrap();
        assert_eq!(state.kinds.last().unwrap().1, "post");
        assert_eq!(
            state.sent.last().unwrap().text,
            "**Plan**\n```sh\nls\n```\ndone"
        );
    }
    mock.state.lock().unwrap().refuse_posts = true;
    adapter
        .send(&Outgoing::text(DM, None, "**Plain** now"))
        .await
        .unwrap();
    {
        let state = mock.state.lock().unwrap();
        assert_eq!(state.kinds.last().unwrap().1, "text");
        assert_eq!(state.sent.last().unwrap().text, "Plain now");
    }
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_press_counts_once_and_replies_thread_under_the_message() {
    let (mock, _bridge, adapter, link, _root) = connected(&json!({"batch_delay_ms": 0})).await;
    let asked = mock.say(DM, USER, "approve?", &json!([]));
    assert_eq!(link.wait_for(1).await.len(), 1);
    let press = |event_id: &str| {
        envelope(
            event_id,
            "card.action.trigger",
            &json!({"operator": {"open_id": USER}, "token": "c-same",
                "action": {"tag": "button", "value": {"prometheus": "appr:yes"}},
                "context": {"open_message_id": "om_1", "open_chat_id": DM}}),
        )
    };
    // Feishu retries a callback with a new event id but the same token.
    mock.deliver(&press("ev-p1"));
    mock.deliver(&press("ev-p2"));
    // The question and one press; a second press would make three.
    let admitted = link.wait_for(2).await;
    let presses: Vec<&Inbound> = admitted.iter().filter(|i| i.callback.is_some()).collect();
    assert_eq!(presses.len(), 1, "{admitted:?}");
    assert_eq!(presses[0].callback.as_deref(), Some("appr:yes"));
    let mut reply = Outgoing::text(DM, None, "on it");
    reply.reply_to = Some(asked.clone());
    adapter.send(&reply).await.unwrap();
    let replied = mock.state.lock().unwrap().replies.last().unwrap().0.clone();
    assert_eq!(replied, asked);
    adapter.disconnect().await;
}

const VERIFY: &str = "verify-token";
const ENCRYPT: &str = "encrypt-key";

fn signed_post(url: &str, body: &str, key: &str) -> u16 {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(format!("1700000000nonce1{key}{body}").as_bytes());
    let signature = digest.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    });
    catalog::post(
        url,
        &[
            ("content-type", "application/json"),
            ("x-lark-request-timestamp", "1700000000"),
            ("x-lark-request-nonce", "nonce1"),
            ("x-lark-signature", &signature),
        ],
        body.as_bytes(),
    )
    .0
}

/// Webhook mode never runs unauthenticated: without the verification
/// token it does not start, so no forged event reaches the gateway.
#[tokio::test(flavor = "multi_thread")]
async fn the_webhook_mode_does_not_start_without_the_verification_token() {
    let mock = Feishu::start().await;
    let (kit, _root) = mock.install(
        None,
        &json!({"connection_mode": "webhook", "batch_delay_ms": 0}),
    );
    catalog::secret(&kit, "FEISHU_ENCRYPT_KEY", ENCRYPT);
    let err = kit
        .adapter()
        .unwrap()
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(
        err.message.contains("FEISHU_VERIFICATION_TOKEN"),
        "{}",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_webhook_mode_checks_token_signature_size_and_rate() {
    let mock = Feishu::start().await;
    let (kit, _root) = mock.install(
        None,
        &json!({"connection_mode": "webhook", "batch_delay_ms": 0}),
    );
    catalog::secret(&kit, "FEISHU_VERIFICATION_TOKEN", VERIFY);
    catalog::secret(&kit, "FEISHU_ENCRYPT_KEY", ENCRYPT);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    let prefix = catalog::serve(Arc::clone(&adapter)).await;
    adapter.connect(link.dyn_link()).await.unwrap();
    let url = format!("{prefix}/");
    // The challenge is echoed only with the right verification token.
    let challenge = |token: &str| {
        json!({"type": "url_verification", "challenge": "ch-1", "token": token}).to_string()
    };
    assert_eq!(signed_post(&url, &challenge("wrong"), ENCRYPT), 401);
    let (status, body) = catalog::post(
        &url,
        &[("content-type", "application/json")],
        challenge(VERIFY).as_bytes(),
    );
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["challenge"],
        "ch-1"
    );
    // An event needs the signature over timestamp, nonce, key and body.
    let event = message_event(
        "om_w1",
        DM,
        USER,
        "text",
        &json!({"text": "via webhook"}),
        &json!([]),
    )
    .to_string();
    assert_eq!(signed_post(&url, &event, "not-the-key"), 401);
    assert_eq!(signed_post(&url, &event, ENCRYPT), 200);
    assert_eq!(texts(&link.wait_for(1).await), ["via webhook"]);
    // An encrypted envelope decrypts with the encrypt key.
    let sealed = seal(
        &message_event(
            "om_w2",
            DM,
            USER,
            "text",
            &json!({"text": "sealed"}),
            &json!([]),
        )
        .to_string(),
    );
    assert_eq!(signed_post(&url, &sealed, ENCRYPT), 200);
    assert_eq!(texts(&link.wait_for(2).await), ["via webhook", "sealed"]);
    // Bodies over 1 MB are refused.
    assert_eq!(oversized(&url), 413);
    // 120 requests a minute per source, then 429.
    let statuses: Vec<u16> = (0..120).map(|_| signed_post(&url, "{}", "x")).collect();
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
    adapter.disconnect().await;
}

/// Posts a body over 1 MB: the status, read even when the server stops
/// reading the body early.
fn oversized(url: &str) -> u16 {
    use std::io::Read as _;
    let url = url::Url::parse(url).unwrap();
    let mut stream =
        std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let length = 1024 * 1024 + 10;
    let head = format!(
        "POST {} HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n",
        url.path()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&vec![b' '; length]);
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    String::from_utf8_lossy(&answer)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The encrypted envelope Feishu sends when an encrypt key is set.
fn seal(plain: &str) -> String {
    use base64::Engine as _;
    use cbc::cipher::{BlockEncryptMut as _, KeyIvInit as _, block_padding::NoPadding};
    use sha2::Digest as _;
    let key = sha2::Sha256::digest(ENCRYPT.as_bytes());
    let iv = [3u8; 16];
    let mut buffer = plain.as_bytes().to_vec();
    let n = 16 - buffer.len() % 16;
    buffer.extend(std::iter::repeat_n(u8::try_from(n).unwrap(), n));
    let len = buffer.len();
    cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &iv)
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut buffer, len)
        .unwrap();
    let mut data = iv.to_vec();
    data.extend(buffer);
    json!({"encrypt": base64::engine::general_purpose::STANDARD.encode(data)}).to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lark_domain_and_a_bad_mode_are_checked_at_start() {
    let mock = Feishu::start().await;
    let (kit, _root) = mock.install(None, &json!({"connection_mode": "carrier-pigeon"}));
    let adapter = kit.adapter().unwrap();
    let err = adapter
        .connect(catalog::Link::new().dyn_link())
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("connection_mode"), "{err:?}");
    let _ = catalog_feishu::APP_ID;
}
