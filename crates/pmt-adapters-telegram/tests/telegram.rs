//! The Telegram adapter against a mock Bot API (spec: platform-adapters):
//! the conformance kit, the profile-scoped token, media in and out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_telegram::mock::{self, BotApi, TOKEN};
use pmt_adapters_telegram::{TOKEN_SECRET, Telegram};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::SecretString;
use pmt_gateway::messaging::conformance::{self, MockPlatform as _};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterSetup, AdapterState, Admission,
    Inbound, MediaKind, Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;

/// A profile of a temporary home enabling Telegram (short long polls),
/// with the mock's token in its secrets unless `token` is false.
fn profile(home: &std::path::Path, name: &str, token: bool) -> AdapterSetup {
    let layout = Home::open(home).unwrap();
    let dir = layout.profile(&ProfileName::parse(name).unwrap()).unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[gateway.platforms.telegram]\nenabled = true\noptions = { poll_timeout_secs = 1 }\n",
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
    let config = scope.config().gateway.platform("telegram");
    AdapterSetup {
        scope,
        platform: "telegram".into(),
        config,
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let (adapter, platform) = mock::kit(home.path()).await.unwrap();
    let report = conformance::run(adapter, platform).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
}

#[tokio::test]
async fn a_secondary_profile_never_uses_the_default_profiles_token() {
    let home = tempfile::tempdir().unwrap();
    let api = BotApi::start().await;
    let _default = profile(home.path(), "default", true);
    let work = Telegram::new(&profile(home.path(), "work", false), api.endpoints()).unwrap();
    let err = work
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "missing_secret");
    assert!(!err.retryable);
    assert!(
        err.message.contains("TELEGRAM_BOT_TOKEN") && err.message.contains("'work'"),
        "{}",
        err.message
    );
    assert_eq!(work.credential_identity(), None);
}

#[tokio::test]
async fn updates_arriving_while_disconnected_arrive_once_after_reconnecting() {
    let home = tempfile::tempdir().unwrap();
    let api = BotApi::start().await;
    let adapter = Telegram::new(&profile(home.path(), "default", true), api.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    api.inject("before");
    eventually(|| link.events.lock().unwrap().len() == 1).await;
    adapter.disconnect().await;
    for text in ["one", "two", "three"] {
        api.inject(text);
    }
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    eventually(|| link.events.lock().unwrap().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
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
async fn photos_download_and_files_upload() {
    let home = tempfile::tempdir().unwrap();
    let api = BotApi::start().await;
    api.add_file("photo-1", vec![1, 2, 3, 4]);
    let setup = profile(home.path(), "default", true);
    let adapter = Telegram::new(&setup, api.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    api.inject_photo("photo-1", "a cat");
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.text, "a cat");
    assert_eq!(event.attachments[0].kind, MediaKind::Image);
    let mut stream = adapter.open_media(&event.attachments[0]).await.unwrap();
    assert_eq!(stream.declared_len, Some(4));
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, [1, 2, 3, 4]);
    let file = home.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    let mut message = Outgoing::text("4242", None, "the chart");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    adapter.send(&message).await.unwrap();
    assert_eq!(api.uploads(), ["sendPhoto"]);
    assert_eq!(api.sent().len(), 2);
}

/// Telegram's adapter claiming the wrong unit (characters, not UTF-16).
struct WrongUnit(Telegram);

impl Adapter for WrongUnit {
    fn describe(&self) -> pmt_gateway::messaging::Descriptor {
        pmt_gateway::messaging::Descriptor {
            length_unit: pmt_gateway::messaging::LengthUnit::Chars,
            ..self.0.describe()
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        self.0.connect(link)
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        self.0.disconnect()
    }

    fn send(
        &self,
        message: &Outgoing,
    ) -> AdapterFuture<'_, Result<pmt_gateway::messaging::Sent, pmt_gateway::messaging::SendError>>
    {
        self.0.send(message)
    }

    fn chat_info<'a>(
        &'a self,
        chat_id: &'a str,
    ) -> AdapterFuture<
        'a,
        Result<pmt_gateway::messaging::ChatInfo, pmt_gateway::messaging::SendError>,
    > {
        self.0.chat_info(chat_id)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_kit_catches_a_wrong_length_unit() {
    let home = tempfile::tempdir().unwrap();
    let api = BotApi::start().await;
    let adapter = Telegram::new(&profile(home.path(), "default", true), api.endpoints()).unwrap();
    let report = conformance::run(Arc::new(WrongUnit(adapter)), Arc::clone(&api) as _).await;
    assert!(!report.passed());
    let splitting = report
        .checks
        .iter()
        .find(|c| c.name == "splitting")
        .unwrap();
    assert!(
        matches!(&splitting.outcome, conformance::Outcome::Fail(why) if why.contains("too long")),
        "{}",
        report.render()
    );
}

/// Telegram's adapter declaring everything but delivering only the
/// required operations, and dropping the thread of every send.
struct Hollow(Telegram);

impl Adapter for Hollow {
    fn describe(&self) -> pmt_gateway::messaging::Descriptor {
        self.0.describe()
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        self.0.connect(link)
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        self.0.disconnect()
    }

    fn send(
        &self,
        message: &Outgoing,
    ) -> AdapterFuture<'_, Result<pmt_gateway::messaging::Sent, pmt_gateway::messaging::SendError>>
    {
        let mut unthreaded = message.clone();
        unthreaded.thread_id = None;
        self.0.send(&unthreaded)
    }

    fn chat_info<'a>(
        &'a self,
        chat_id: &'a str,
    ) -> AdapterFuture<
        'a,
        Result<pmt_gateway::messaging::ChatInfo, pmt_gateway::messaging::SendError>,
    > {
        self.0.chat_info(chat_id)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_kit_fails_each_declared_capability_the_adapter_does_not_deliver() {
    let home = tempfile::tempdir().unwrap();
    let api = BotApi::start().await;
    let adapter = Telegram::new(&profile(home.path(), "default", true), api.endpoints()).unwrap();
    let report = conformance::run(Arc::new(Hollow(adapter)), Arc::clone(&api) as _).await;
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, conformance::Outcome::Fail(_)))
        .map(|c| c.name)
        .collect();
    assert_eq!(
        failed,
        [
            "edit",
            "typing",
            "reactions",
            "threads",
            "delete",
            "media in"
        ],
        "{}",
        report.render()
    );
}
