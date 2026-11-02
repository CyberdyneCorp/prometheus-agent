//! The Discord adapter against a mock REST API and Gateway (spec:
//! platform-adapters): the conformance kit, resume after a dropped
//! connection, the Message Content intent, media in and out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_discord::mock::{self, DiscordMock, TOKEN};
use pmt_adapters_discord::{Discord, TOKEN_SECRET};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::SecretString;
use pmt_gateway::messaging::conformance::{self, MockPlatform as _};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterSetup, AdapterState, Admission,
    Inbound, MediaKind, Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;

fn profile(home: &std::path::Path, token: bool) -> AdapterSetup {
    let layout = Home::open(home).unwrap();
    let dir = layout
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[gateway.platforms.discord]\nenabled = true\n",
    )
    .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    if token {
        scope
            .secrets()
            .store()
            .set(TOKEN_SECRET, &SecretString::new(TOKEN))
            .unwrap();
    }
    let config = scope.config().gateway.platform("discord");
    AdapterSetup {
        scope,
        platform: "discord".into(),
        config,
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let report = conformance::run(Arc::new(adapter), Arc::clone(&mock) as _).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    assert_eq!(mock.handshakes(), (1, 1), "one identify, then a resume");
}

#[tokio::test]
async fn a_lost_connection_resumes_and_replays_what_was_missed() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    mock.inject("before");
    eventually(|| link.events.lock().unwrap().len() == 1).await;
    mock.drop_connection();
    eventually(|| {
        link.states
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| *s == AdapterState::Retrying)
    })
    .await;
    for text in ["one", "two", "three"] {
        mock.inject(text);
    }
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    eventually(|| link.events.lock().unwrap().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let texts: Vec<String> = link
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(texts, ["before", "one", "two", "three"]);
}

#[tokio::test]
async fn without_the_message_content_intent_the_connection_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    mock.refuse_identify(4014);
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let err = adapter
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert!(!err.retryable);
    assert!(
        err.message.contains("enable the Message Content intent"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn a_missing_token_is_a_fatal_missing_secret() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), false), mock.endpoints()).unwrap();
    let err = adapter
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "missing_secret");
    assert!(err.message.contains("DISCORD_BOT_TOKEN"));
}

#[tokio::test]
async fn files_download_from_the_cdn_and_upload_as_attachments() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    let url = mock.add_file("report.pdf", vec![9; 10]);
    mock.inject_file(&url, "report.pdf", "application/pdf");
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Document);
    assert_eq!(event.attachments[0].name.as_deref(), Some("report.pdf"));
    let mut stream = adapter.open_media(&event.attachments[0]).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, vec![9; 10]);
    let file = home.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    let mut message = Outgoing::text(mock::DM, None, "the chart");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    adapter.send(&message).await.unwrap();
    assert_eq!(mock.uploads(), 1);
    assert_eq!(mock.sent().last().unwrap().text, "the chart");
}
