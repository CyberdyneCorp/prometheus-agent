//! The Slack adapter against a mock Web API and Socket Mode server (spec:
//! platform-adapters): the conformance kit, profile-scoped tokens, files
//! in and out, cards edited after resolution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_slack::mock::{self, APP_TOKEN, BOT_TOKEN, SlackMock};
use pmt_adapters_slack::{APP_TOKEN_SECRET, Slack, TOKEN_SECRET};
use pmt_gateway::messaging::conformance::{self, MockPlatform as _, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Button, Inbound,
    MediaKind, Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;
use serde_json::json;

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn adapter(home: &std::path::Path, mock: &SlackMock) -> Slack {
    let setup = kit_setup(
        home,
        "slack",
        "",
        &[(TOKEN_SECRET, BOT_TOKEN), (APP_TOKEN_SECRET, APP_TOKEN)],
    )
    .unwrap();
    Slack::new(&setup, mock.endpoints()).unwrap()
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
async fn a_missing_app_token_is_named_for_the_profile() {
    let home = tempfile::tempdir().unwrap();
    let mock = SlackMock::start().await;
    let setup = kit_setup(home.path(), "slack", "", &[(TOKEN_SECRET, BOT_TOKEN)]).unwrap();
    let slack = Slack::new(&setup, mock.endpoints()).unwrap();
    let err = slack
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "missing_secret");
    assert!(!err.retryable);
    assert!(err.message.contains("SLACK_APP_TOKEN") && err.message.contains("'default'"));
}

#[tokio::test]
async fn files_download_with_the_bot_token_and_upload_into_the_conversation() {
    let home = tempfile::tempdir().unwrap();
    let mock = SlackMock::start().await;
    let url = mock.add_file("cat.png", vec![9, 8, 7]);
    let slack = adapter(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    mock.inject_event(json!({"text": "a cat", "subtype": "file_share",
        "files": [{"name": "cat.png", "mimetype": "image/png", "url_private_download": url}]}));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Image);
    let mut stream = slack.open_media(&event.attachments[0]).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, [9, 8, 7]);
    let file = home.path().join("report.pdf");
    std::fs::write(&file, b"%PDF").unwrap();
    let mut message = Outgoing::text(mock::DM, None, "the report");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    slack.send(&message).await.unwrap();
    assert_eq!(mock.uploads(), ["report.pdf"]);
    slack.disconnect().await;
}

#[tokio::test]
async fn a_resolved_card_loses_its_buttons() {
    let home = tempfile::tempdir().unwrap();
    let mock = SlackMock::start().await;
    let slack = adapter(home.path(), &mock);
    let mut card = Outgoing::text(mock::DM, None, "Approval needed: terminal");
    card.buttons = vec![Button {
        label: "Once".into(),
        data: "appr:srq-1:once:mac".into(),
    }];
    let id = slack.send(&card).await.unwrap().message_id.unwrap();
    slack
        .edit(
            &id,
            &Outgoing::text(mock::DM, None, "Approved once by kit-user"),
        )
        .await
        .unwrap();
    let shown = mock
        .sent()
        .into_iter()
        .find(|m| m.message_id == id)
        .unwrap();
    assert_eq!(shown.text, "Approved once by kit-user");
    assert!(shown.buttons.is_empty());
}

#[tokio::test]
async fn unacknowledged_envelopes_arrive_once_after_reconnecting() {
    let home = tempfile::tempdir().unwrap();
    let mock = SlackMock::start().await;
    let slack = adapter(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    slack.disconnect().await;
    eventually(|| mock.connections() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for text in ["one", "two", "three"] {
        mock.inject(text);
    }
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    eventually(|| link.events.lock().unwrap().len() == 3).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let texts: Vec<String> = link
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(texts, ["one", "two", "three"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slash_command_runs_and_is_answered_privately() {
    let home = tempfile::tempdir().unwrap();
    let mock = SlackMock::start().await;
    let slack = adapter(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    let menu: Vec<pmt_gateway::messaging::MenuCommand> = ["model", "stop"]
        .iter()
        .map(|n| pmt_gateway::messaging::MenuCommand {
            name: (*n).into(),
            description: String::new(),
            takes_args: true,
        })
        .collect();
    slack.register_commands(&menu).await.unwrap();
    mock.slash("/prometheus", "model fast");
    mock.slash("/prometheus", "how are you");
    eventually(|| link.events.lock().unwrap().len() == 2).await;
    let events = link.events.lock().unwrap().clone();
    assert_eq!(events[0].text, "/model fast");
    assert_eq!(events[1].text, "how are you");
    eventually(|| mock.ack_payloads().len() == 2).await;
    assert_eq!(mock.ack_payloads()[0]["text"], "Running /model…");
    assert_eq!(mock.ack_payloads()[0]["response_type"], "ephemeral");

    let mut answer = Outgoing::text(mock::DM, None, "Model set to fast.");
    answer.reply_to = Some(events[0].message_id.clone());
    slack.send(&answer).await.unwrap();
    let responses = mock.responses();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["text"], "Model set to fast.");
    assert_eq!(responses[0]["response_type"], "ephemeral");
    assert_eq!(responses[0]["replace_original"], true);
    assert!(
        mock.sent().iter().all(|m| m.text != "Model set to fast."),
        "never a public post"
    );
    slack.disconnect().await;
}
