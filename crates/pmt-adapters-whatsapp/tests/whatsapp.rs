//! The `WhatsApp` adapter against a mock bridge (spec: platform-adapters):
//! the conformance kit, the alias flip, logout, the missing runtime,
//! media.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_whatsapp::Whatsapp;
use pmt_adapters_whatsapp::mock::{self, BridgeMock};
use pmt_gateway::messaging::conformance::{self, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, MediaKind,
    Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;
use serde_json::json;

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
    states: Mutex<Vec<(AdapterState, Option<AdapterError>)>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, state: AdapterState, error: Option<AdapterError>) {
        self.states.lock().unwrap().push((state, error));
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connected(home: &std::path::Path) -> (Arc<BridgeMock>, Whatsapp, Arc<Recorder>) {
    let mock = BridgeMock::start().await;
    let setup = kit_setup(home, "whatsapp", &mock.table(), &[]).unwrap();
    let adapter = Whatsapp::new(&setup).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    (mock, adapter, link)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let (adapter, platform) = mock::kit(home.path()).await.unwrap();
    let report = conformance::run(adapter, platform).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
}

#[tokio::test]
async fn whatsapp_alias_flip_keeps_one_sender_and_one_chat() {
    let home = tempfile::tempdir().unwrap();
    let (mock, adapter, link) = connected(home.path()).await;
    mock.receive(
        &json!({"body": "by phone", "chatId": "15551234567@s.whatsapp.net",
        "senderId": "15551234567@s.whatsapp.net", "senderAltId": "98765@lid"}),
    );
    mock.receive(&json!({"body": "by lid", "chatId": "98765@lid", "senderId": "98765@lid"}));
    eventually(|| link.events.lock().unwrap().len() == 2).await;
    let events = link.events.lock().unwrap().clone();
    assert_eq!(events[0].sender_id, events[1].sender_id);
    assert_eq!(events[0].chat_id, events[1].chat_id);
    adapter.disconnect().await;
}

#[tokio::test]
async fn a_logged_out_device_is_fatal_naming_the_pair_command() {
    let home = tempfile::tempdir().unwrap();
    let (mock, adapter, link) = connected(home.path()).await;
    mock.log_out();
    eventually(|| !link.states.lock().unwrap().is_empty()).await;
    let (state, error) = link.states.lock().unwrap()[0].clone();
    assert_eq!(state, AdapterState::Fatal);
    let error = error.unwrap();
    assert_eq!(error.code, "whatsapp_logged_out");
    assert!(
        error
            .message
            .contains("prometheus -p default gateway whatsapp pair")
    );
    let again = adapter.connect(Arc::clone(&link) as _).await.unwrap_err();
    assert_eq!(again.code, "whatsapp_logged_out");
}

#[tokio::test]
async fn without_a_bridge_url_a_missing_runtime_is_fatal() {
    let home = tempfile::tempdir().unwrap();
    let setup = kit_setup(home.path(), "whatsapp", "", &[]).unwrap();
    let err = Whatsapp::new(&setup)
        .unwrap()
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "whatsapp_bridge_missing");
    assert!(!err.retryable);
    assert!(
        err.message.contains("gateway whatsapp pair"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn media_is_read_from_the_bridge_cache_and_sent_by_kind() {
    let home = tempfile::tempdir().unwrap();
    let (mock, adapter, link) = connected(home.path()).await;
    let cached = home.path().join("voice.ogg");
    std::fs::write(&cached, b"OggS").unwrap();
    mock.receive(&json!({"mediaType": "ptt", "mediaUrls": [cached.to_string_lossy()]}));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Voice);
    let mut stream = adapter.open_media(&event.attachments[0]).await.unwrap();
    let chunk = futures_util::StreamExt::next(&mut stream.chunks)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk, b"OggS");
    let mut message = Outgoing::text(mock::USER, None, "");
    message.attachments = vec![
        OutboundAttachment::new(&home.path().join("chart.png").to_string_lossy()),
        OutboundAttachment {
            as_voice: true,
            ..OutboundAttachment::new(&cached.to_string_lossy())
        },
    ];
    adapter.send(&message).await.unwrap();
    let kinds: Vec<String> = mock.media().into_iter().map(|(_, k)| k).collect();
    assert_eq!(kinds, ["image", "audio"]);
    adapter.disconnect().await;
}

fn clarify_card() -> Outgoing {
    let mut card = Outgoing::text(mock::USER, None, "Which colour?\n1. red\n2. green\n3. blue");
    card.buttons = ["red", "green", "blue"]
        .iter()
        .enumerate()
        .map(|(i, c)| pmt_gateway::messaging::Button {
            label: (*c).into(),
            data: format!("clar:req1:0.{}:mac", i + 1),
        })
        .collect();
    card
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clarify_question_is_a_poll_whose_vote_is_the_answer() {
    let home = tempfile::tempdir().unwrap();
    let (mock, adapter, link) = connected(home.path()).await;
    let sent = adapter.send(&clarify_card()).await.unwrap();
    let polls = mock.polls();
    assert_eq!(polls.len(), 1);
    let (id, _, question, options, selectable) = polls[0].clone();
    assert_eq!(sent.message_id.as_deref(), Some(id.as_str()));
    assert_eq!(question, "Which colour?");
    assert_eq!(options, ["red", "green", "blue"]);
    assert_eq!(selectable, 1);
    mock.vote(&id, &["green"]);
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let vote = link.events.lock().unwrap()[0].clone();
    assert_eq!(vote.callback.as_deref(), Some("clar:req1:0.2:mac"));
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_poll_falls_back_to_numbered_text() {
    let home = tempfile::tempdir().unwrap();
    let (mock, adapter, _link) = connected(home.path()).await;
    mock.refuse_polls();
    adapter.send(&clarify_card()).await.unwrap();
    let sent = conformance::MockPlatform::sent(mock.as_ref());
    assert!(sent.iter().any(|m| m.text.contains("2. green")), "{sent:?}");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn self_chat_mode_ignores_contacts() {
    let home = tempfile::tempdir().unwrap();
    let mock = BridgeMock::start().await;
    let table = mock.table().replace(" }", ", mode = \"self_chat\" }");
    let setup = kit_setup(home.path(), "whatsapp", &table, &[]).unwrap();
    let adapter = Whatsapp::new(&setup).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    let me = "15550001111@s.whatsapp.net";
    mock.receive(&json!({"body": "hi from a friend"}));
    mock.receive(&json!({"chatId": me, "senderId": me, "fromMe": true, "body": "note to me"}));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let texts: Vec<String> = link
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(texts, ["note to me"]);
    adapter.disconnect().await;
}
