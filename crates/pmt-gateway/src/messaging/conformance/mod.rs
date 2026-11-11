//! The adapter conformance kit (spec: platform-adapters, Adapter
//! conformance kit): one suite every adapter must pass against a mock of
//! its platform. It connects and reconnects the adapter, checks its
//! descriptor, inbound normalization and deduplication, splitting at the
//! declared limit and unit (the mock enforces the platform's real one),
//! send retry classification, each declared optional capability (every
//! descriptor capability gets a pass, fail or skip line; see
//! [`capabilities`]) and the card rendering of its interactive tier. A
//! platform's mock server implements [`MockPlatform`];
//! `prometheus plugins test-adapter` runs the same suite.

mod capabilities;

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::adapter::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Button, Inbound,
    Outgoing, Reaction, SendError,
};
use super::dedup::Dedup;
use super::descriptor::{Descriptor, Interactive, Markup, MediaKind};
use super::key::ChatType;
use super::split;

/// How long the kit waits for something the platform should deliver.
const WAIT: Duration = Duration::from_secs(15);

/// A failure the mock platform answers the next send with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Throttled, with the platform's `retry_after` in seconds.
    RateLimited(u64),
    /// A 5xx.
    ServerError,
    /// The message's markup is malformed.
    Markup,
    /// The platform refuses the message for good.
    Forbidden,
}

/// A message the mock platform accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockSent {
    /// Chat id.
    pub chat_id: String,
    /// Platform message id.
    pub message_id: String,
    /// The text (after edits).
    pub text: String,
    /// Its buttons (`label`, `data`).
    pub buttons: Vec<Button>,
    /// The thread it landed in.
    pub thread_id: Option<String>,
}

/// A file the mock platform accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockFile {
    /// Chat id.
    pub chat_id: String,
    /// The thread it landed in.
    pub thread_id: Option<String>,
    /// Its file name.
    pub name: String,
    /// What the platform shows it as (a platform that types files by
    /// name or MIME type shows what [`kind_of_name`] says).
    pub kind: MediaKind,
    /// Its content.
    pub bytes: Vec<u8>,
}

/// The media kind a platform that types files by name shows a file as.
#[must_use]
pub fn kind_of_name(name: &str) -> MediaKind {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => MediaKind::Image,
        Some("ogg" | "oga" | "opus" | "mp3" | "m4a" | "wav" | "flac") => MediaKind::Audio,
        Some("mp4" | "mov" | "webm") => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

/// The MIME type of a file the kit sends or injects, by name.
#[must_use]
pub fn mime_of_name(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// A platform mock the kit drives.
pub trait MockPlatform: Send + Sync {
    /// The direct-message chat the kit talks in, and its user.
    fn dm(&self) -> (String, String);
    /// The platform delivers a text message in the DM; its message id.
    fn inject(&self, text: &str) -> String;
    /// The platform delivers the last injected message again.
    fn redeliver(&self);
    /// The platform drops the adapter's connection (messages injected
    /// meanwhile wait server side).
    fn drop_connection(&self);
    /// The user presses a button of message `message_id`.
    fn press(&self, message_id: &str, data: &str);
    /// The next send fails with `failure`.
    fn fail_next(&self, failure: Failure);
    /// Whether the platform reports malformed markup as its own error.
    fn reports_markup_errors(&self) -> bool;
    /// Every message the platform accepted, in order.
    fn sent(&self) -> Vec<MockSent>;
    /// Typing indicators shown.
    fn typing(&self) -> usize;
    /// Reactions added: (message id, emoji).
    fn reactions(&self) -> Vec<(String, String)>;
    /// The kit serves a `binds_http` adapter at `base` (an
    /// `http://host:port/p/<profile>/<platform>` prefix); the platform
    /// posts its requests there.
    fn listen(&self, _base: &str) {}
    /// Whether the platform's throttling names a wait (`retry_after`); a
    /// platform without one (SMTP) is expected to report `rate_limited`
    /// without a hint.
    fn hints_retry_after(&self) -> bool {
        true
    }
    /// Whether sends can fail at all: an adapter that only records its
    /// replies (webhook `log` delivery) has no failures to classify.
    fn sends_can_fail(&self) -> bool {
        true
    }
    /// A chat and one of its threads the kit may post in; `None` when the
    /// mock has none (an adapter declaring `supports_threads` then fails).
    fn thread(&self) -> Option<(String, String)> {
        None
    }
    /// The platform delivers a DM from the user carrying `caption` and one
    /// file of `kind` (as the platform sends that kind: a voice note as
    /// one, where it has them) named `name` holding `bytes`; its message
    /// id, or `None` when the mock cannot deliver files.
    fn inject_media(
        &self,
        _kind: MediaKind,
        _name: &str,
        _bytes: &[u8],
        _caption: &str,
    ) -> Option<String> {
        None
    }
    /// The platform delivers a location pin (or live location) named
    /// `name` in the DM; its message id, or `None` when the mock cannot.
    fn inject_location(
        &self,
        _latitude: f64,
        _longitude: f64,
        _name: &str,
        _live: bool,
    ) -> Option<String> {
        None
    }
    /// Location pins the platform accepted: `(chat, latitude, longitude,
    /// name)`.
    fn locations(&self) -> Vec<(String, f64, f64, Option<String>)> {
        Vec::new()
    }
    /// Replies only one member saw (ephemeral messages): `(member, text)`.
    /// A private reply sent as a direct message shows in [`Self::sent`].
    fn private_replies(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Every file the platform accepted, in order.
    fn files(&self) -> Vec<MockFile> {
        Vec::new()
    }
    /// Whether a bot can post a voice note distinct from an audio file;
    /// where it cannot (Slack, signal-cli), a voice reply is
    /// expected to arrive as an audio file.
    fn voice_notes_out(&self) -> bool {
        true
    }
    /// Ids of the messages deleted.
    fn deleted(&self) -> Vec<String> {
        Vec::new()
    }
    /// Streamed audio the platform played, one entry per finished stream.
    fn streamed_audio(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    /// The names in the platform's native command menu as last
    /// registered; `None` for a platform without one.
    fn menu(&self) -> Option<Vec<String>> {
        None
    }
}

/// One check's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It passed.
    Pass,
    /// It failed: why.
    Fail(String),
    /// It does not apply: why.
    Skip(String),
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Its name.
    pub name: &'static str,
    /// What happened.
    pub outcome: Outcome,
}

/// The suite's result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every check, in order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether no check failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|c| matches!(c.outcome, Outcome::Fail(_)))
    }

    /// One `pass`/`fail`/`skip` line per check.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        for check in &self.checks {
            let _ = match &check.outcome {
                Outcome::Pass => writeln!(text, "pass  {}", check.name),
                Outcome::Fail(why) => writeln!(text, "FAIL  {}: {why}", check.name),
                Outcome::Skip(why) => writeln!(text, "skip  {}: {why}", check.name),
            };
        }
        text
    }

    fn add(&mut self, name: &'static str, outcome: Result<(), String>) {
        let outcome = match outcome {
            Ok(()) => Outcome::Pass,
            Err(why) => Outcome::Fail(why),
        };
        self.checks.push(Check { name, outcome });
    }

    fn skip(&mut self, name: &'static str, why: &str) {
        self.checks.push(Check {
            name,
            outcome: Outcome::Skip(why.to_owned()),
        });
    }
}

/// The gateway as the kit plays it: inbound events through the host's
/// deduplication, connection losses noticed.
#[derive(Default)]
struct KitLink {
    received: Mutex<Vec<Inbound>>,
    dedup: Dedup,
    lost: Notify,
    states: Mutex<Vec<AdapterState>>,
}

impl KitLink {
    fn accepted(&self) -> Vec<Inbound> {
        self.received.lock().expect("kit poisoned").clone()
    }
}

impl AdapterLink for KitLink {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        let duplicate = self.dedup.seen(
            "kit",
            &inbound.chat_id,
            &inbound.message_id,
            Duration::from_mins(5),
            2000,
            tokio::time::Instant::now(),
        );
        if !duplicate {
            self.received.lock().expect("kit poisoned").push(inbound);
        }
        Box::pin(async move {
            if duplicate {
                Admission::Duplicate
            } else {
                Admission::Accepted
            }
        })
    }

    fn report(&self, state: AdapterState, _error: Option<AdapterError>) {
        self.states.lock().expect("kit poisoned").push(state);
        if matches!(state, AdapterState::Retrying | AdapterState::Fatal) {
            self.lost.notify_one();
        }
    }
}

async fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// Runs the suite: `adapter` must be built for `platform`'s mock and not
/// yet connected.
pub async fn run(adapter: Arc<dyn Adapter>, platform: Arc<dyn MockPlatform>) -> Report {
    let mut report = Report::default();
    let descriptor = adapter.describe();
    report.add("descriptor", check_descriptor(&descriptor));
    let link = Arc::new(KitLink::default());
    if descriptor.binds_http {
        match listen(Arc::clone(&adapter)).await {
            Ok(base) => platform.listen(&base),
            Err(why) => {
                report.add("connect", Err(why));
                return report;
            }
        }
    }
    let connected = connect(&adapter, &link).await;
    let ok = connected.is_ok();
    report.add("connect", connected);
    if !ok {
        return report;
    }
    let kit = Kit {
        adapter,
        platform,
        link,
        descriptor,
    };
    let keeper = pmt_core::spawn::spawn(kit.reconnecting());
    report.add("inbound normalization", kit.inbound().await);
    report.add("deduplication", kit.deduplication().await);
    report.add("reconnect", kit.reconnect().await);
    report.add("splitting", kit.splitting().await);
    if kit.platform.sends_can_fail() {
        report.add("retry classification", kit.retries().await);
    } else {
        report.skip(
            "retry classification",
            "the adapter records its replies; no send can fail",
        );
    }
    kit.optional(&mut report).await;
    kit.cards(&mut report).await;
    report.skip(
        "capability grants",
        "first-party adapters run in the host without plugin grants",
    );
    keeper.abort();
    kit.adapter.disconnect().await;
    report
}

fn check_descriptor(descriptor: &Descriptor) -> Result<(), String> {
    descriptor.check()?;
    let value = serde_json::to_value(descriptor).map_err(|e| e.to_string())?;
    let read = Descriptor::from_value(&value)?;
    if &read == descriptor {
        Ok(())
    } else {
        Err("the descriptor does not survive its own JSON form".to_owned())
    }
}

/// Serves `adapter` on a loopback port the way the daemon's shared
/// listener does (signature checked first); the prefix it answers under.
async fn listen(adapter: Arc<dyn Adapter>) -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("cannot listen for the adapter: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("cannot listen for the adapter: {e}"))?;
    let prefix = format!("/p/kit/{}", adapter.describe().platform);
    let base = format!("http://{addr}{prefix}");
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            pmt_core::spawn::spawn(serve_connection(
                Arc::clone(&adapter),
                prefix.clone(),
                stream,
            ));
        }
    });
    Ok(base)
}

async fn serve_connection(
    adapter: Arc<dyn Adapter>,
    prefix: String,
    stream: tokio::net::TcpStream,
) {
    let service = hyper::service::service_fn(move |request| {
        let (adapter, prefix) = (Arc::clone(&adapter), prefix.clone());
        async move {
            let answer = route_kit(adapter.as_ref(), &prefix, request).await;
            Ok::<_, std::convert::Infallible>(super::http::respond(answer))
        }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
        .await;
}

/// A request below the kit's prefix to the adapter; 404 elsewhere.
async fn route_kit(
    adapter: &dyn Adapter,
    prefix: &str,
    request: hyper::Request<hyper::body::Incoming>,
) -> super::adapter::HttpResponse {
    let path = request.uri().path().to_owned();
    let Some(rest) = path.strip_prefix(prefix) else {
        return super::adapter::HttpResponse::status(404);
    };
    let rest = if rest.is_empty() { "/" } else { rest };
    super::http::deliver(adapter, rest.to_owned(), request).await
}

async fn connect(adapter: &Arc<dyn Adapter>, link: &Arc<KitLink>) -> Result<(), String> {
    let link: Arc<dyn AdapterLink> = Arc::clone(link) as Arc<dyn AdapterLink>;
    match tokio::time::timeout(WAIT, adapter.connect(link)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(format!("{} ({})", err.message, err.code)),
        Err(_) => Err("connect did not finish within 15 seconds".to_owned()),
    }
}

struct Kit {
    adapter: Arc<dyn Adapter>,
    platform: Arc<dyn MockPlatform>,
    link: Arc<KitLink>,
    descriptor: Descriptor,
}

impl Kit {
    fn dm(&self) -> String {
        self.platform.dm().0
    }

    /// Connects again whenever the adapter reports a lost connection, as
    /// the gateway's supervisor does.
    fn reconnecting(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (adapter, link) = (Arc::clone(&self.adapter), Arc::clone(&self.link));
        async move {
            loop {
                link.lost.notified().await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let as_link: Arc<dyn AdapterLink> = Arc::clone(&link) as Arc<dyn AdapterLink>;
                let _ = adapter.connect(as_link).await;
            }
        }
    }

    fn texts(&self, text: &str) -> usize {
        self.link
            .accepted()
            .iter()
            .filter(|i| i.text == text)
            .count()
    }

    async fn inbound(&self) -> Result<(), String> {
        let id = self.platform.inject("conformance hello");
        if !eventually(|| self.texts("conformance hello") > 0).await {
            return Err("the injected message never arrived".to_owned());
        }
        let inbound = self
            .link
            .accepted()
            .into_iter()
            .find(|i| i.text == "conformance hello")
            .ok_or("lost")?;
        let (chat, user) = self.platform.dm();
        let problems = [
            (
                inbound.message_id != id,
                "message_id is not the platform's id",
            ),
            (inbound.chat_id != chat, "chat_id is not the DM's id"),
            (inbound.sender_id != user, "sender_id is not the user's id"),
            (
                inbound.chat_type != ChatType::Dm,
                "chat_type of a DM is not dm",
            ),
            (inbound.is_bot, "a human sender is marked as a bot"),
        ];
        match problems.iter().find(|(bad, _)| *bad) {
            Some((_, why)) => Err((*why).to_owned()),
            None => Ok(()),
        }
    }

    async fn deduplication(&self) -> Result<(), String> {
        self.platform.inject("conformance once");
        if !eventually(|| self.texts("conformance once") == 1).await {
            return Err("the message never arrived".to_owned());
        }
        self.platform.redeliver();
        tokio::time::sleep(Duration::from_secs(1)).await;
        match self.texts("conformance once") {
            1 => Ok(()),
            n => Err(format!(
                "a redelivered message was admitted {n} times (its message id changed)"
            )),
        }
    }

    async fn reconnect(&self) -> Result<(), String> {
        self.platform.drop_connection();
        let texts = ["while down 1", "while down 2", "while down 3"];
        for text in texts {
            self.platform.inject(text);
        }
        let all = eventually(|| texts.iter().all(|t| self.texts(t) >= 1)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let counts: Vec<usize> = texts.iter().map(|t| self.texts(t)).collect();
        if all && counts.iter().all(|n| *n == 1) {
            Ok(())
        } else {
            Err(format!(
                "messages sent while disconnected arrived {counts:?} times (want exactly once each)"
            ))
        }
    }

    async fn splitting(&self) -> Result<(), String> {
        let limit = self.descriptor.max_message_length;
        let long: String = (0..limit * 2)
            .map(|i| if i % 3 == 0 { '\u{1f600}' } else { 'a' })
            .collect();
        let parts = if self.descriptor.splits_long_messages {
            vec![long.clone()]
        } else {
            split::split(&long, limit, self.descriptor.length_unit)
        };
        let chat = self.dm();
        for (i, part) in parts.iter().enumerate() {
            let message = Outgoing::text(&chat, None, part);
            if let Err(err) = self.adapter.send(&message).await {
                return Err(format!(
                    "part {} of {} at the declared limit ({limit} {:?}) was refused: {}",
                    i + 1,
                    parts.len(),
                    self.descriptor.length_unit,
                    err.describe()
                ));
            }
        }
        Ok(())
    }

    async fn retries(&self) -> Result<(), String> {
        let chat = self.dm();
        let hinted = self.platform.hints_retry_after();
        let message = Outgoing::text(&chat, None, "conformance retry");
        let mut cases = vec![
            (Failure::RateLimited(7), "rate_limited with retry_after 7s"),
            (Failure::ServerError, "server_error"),
            (Failure::Forbidden, "a permanent refusal"),
        ];
        if self.descriptor.markup != Markup::Plain && self.platform.reports_markup_errors() {
            cases.push((Failure::Markup, "formatting"));
        }
        for (failure, want) in cases {
            self.platform.fail_next(failure);
            let got = self.adapter.send(&message).await;
            let right = match (failure, &got) {
                (Failure::RateLimited(secs), Err(SendError::RateLimited { retry_after })) => {
                    *retry_after == hinted.then(|| Duration::from_secs(secs))
                }
                (Failure::ServerError, Err(SendError::Server(_)))
                | (Failure::Forbidden, Err(SendError::Permanent(_)))
                | (Failure::Markup, Err(SendError::Formatting(_))) => true,
                _ => false,
            };
            if !right {
                return Err(format!("{failure:?} should be {want}, got {got:?}"));
            }
        }
        Ok(())
    }

    async fn optional(&self, report: &mut Report) {
        let chat = self.dm();
        match self.adapter.chat_info(&chat).await {
            Ok(info) if info.chat_type == ChatType::Dm => report.add("chat info", Ok(())),
            other => report.add("chat info", Err(format!("chat-info of the DM: {other:?}"))),
        }
        if self.descriptor.supports_edit {
            report.add("edit", self.edit(&chat).await);
        } else {
            report.skip("edit", "not declared");
        }
        if self.descriptor.supports_typing {
            let before = self.platform.typing();
            let shown = self.adapter.send_typing(&chat, None).await;
            let outcome = match shown {
                Ok(()) if self.platform.typing() > before => Ok(()),
                other => Err(format!("send_typing: {other:?}")),
            };
            report.add("typing", outcome);
        } else {
            report.skip("typing", "not declared");
        }
        if self.descriptor.supports_reactions {
            report.add("reactions", self.react(&chat).await);
        } else {
            report.skip("reactions", "not declared");
        }
        self.capabilities(report).await;
    }

    async fn edit(&self, chat: &str) -> Result<(), String> {
        let sent = self
            .adapter
            .send(&Outgoing::text(chat, None, "before edit"))
            .await
            .map_err(|e| e.describe())?;
        let id = sent.message_id.ok_or("the send returned no message id")?;
        self.adapter
            .edit(&id, &Outgoing::text(chat, None, "after edit"))
            .await
            .map_err(|e| e.describe())?;
        let edited = self
            .platform
            .sent()
            .iter()
            .any(|m| m.message_id == id && m.text == "after edit");
        edited
            .then_some(())
            .ok_or_else(|| "the platform shows no edit".to_owned())
    }

    async fn react(&self, chat: &str) -> Result<(), String> {
        let sent = self
            .adapter
            .send(&Outgoing::text(chat, None, "react to me"))
            .await
            .map_err(|e| e.describe())?;
        let id = sent.message_id.ok_or("the send returned no message id")?;
        let reaction = Reaction {
            chat_id: chat.to_owned(),
            message_id: id.clone(),
            emoji: "\u{1f440}".to_owned(),
            remove: false,
        };
        self.adapter
            .react(&reaction)
            .await
            .map_err(|e| e.describe())?;
        let shown = self.platform.reactions().iter().any(|(m, _)| *m == id);
        shown
            .then_some(())
            .ok_or_else(|| "the platform shows no reaction".to_owned())
    }

    async fn cards(&self, report: &mut Report) {
        if self.descriptor.interactive != Interactive::Native {
            report.skip(
                "cards",
                "choices are rendered as text by the host on this tier",
            );
            return;
        }
        report.add("cards", self.native_card().await);
    }

    async fn native_card(&self) -> Result<(), String> {
        let chat = self.dm();
        let data = "appr:srq-0123456789ab:session:0123456789abcdef0123456789abcdef";
        let mut card = Outgoing::text(&chat, None, "Approval needed: terminal");
        card.buttons = ["Once", "Session"]
            .iter()
            .map(|label| Button {
                label: (*label).to_owned(),
                data: data.replace("session", &label.to_lowercase()),
            })
            .collect();
        let sent = self.adapter.send(&card).await.map_err(|e| e.describe())?;
        let id = sent.message_id.ok_or("the card returned no message id")?;
        let shown = self
            .platform
            .sent()
            .into_iter()
            .find(|m| m.message_id == id)
            .ok_or("the card never arrived")?;
        // A platform without buttons (WhatsApp approvals) numbers the
        // choices and maps a numbered reply back to the button.
        let numbered = card
            .buttons
            .iter()
            .enumerate()
            .all(|(i, b)| shown.text.contains(&format!("{}. {}", i + 1, b.label)));
        if shown.buttons == card.buttons {
            self.platform.press(&id, data);
        } else if numbered {
            let number = card
                .buttons
                .iter()
                .position(|b| b.data == data)
                .unwrap_or(0)
                + 1;
            self.platform.inject(&number.to_string());
        } else {
            return Err("the platform does not show the card's buttons".to_owned());
        }
        let pressed = eventually(|| {
            self.link
                .accepted()
                .iter()
                .any(|i| i.callback.as_deref() == Some(data))
        })
        .await;
        pressed
            .then_some(())
            .ok_or_else(|| "a button press never arrived as a callback".to_owned())
    }
}

/// Profile `default` of the home at `home`, enabling `platform` with the
/// table lines `table` (TOML under `[gateway.platforms.<platform>]`) and
/// holding `secrets` in its store: what the kit builds an adapter from.
///
/// # Errors
///
/// The home cannot be laid out, the config is invalid or a secret cannot
/// be stored.
pub fn kit_setup(
    home: &std::path::Path,
    platform: &str,
    table: &str,
    secrets: &[(&str, &str)],
) -> Result<super::AdapterSetup, pmt_core::error::Error> {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    let layout = Home::open(home)?;
    let dir = layout.profile(&ProfileName::parse("default")?)?;
    let config = format!("[gateway.platforms.{platform}]\nenabled = true\n{table}\n");
    let file = dir.path().join("config.toml");
    std::fs::write(&file, config)
        .map_err(|e| pmt_core::error::Error::io(&pmt_core::error::codes::HOME_IO, &file, &e))?;
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other)?;
    for (name, value) in secrets {
        scope
            .secrets()
            .store()
            .set(name, &pmt_core::secret::SecretString::new(*value))?;
    }
    let config = scope.config().gateway.platform(platform);
    Ok(super::AdapterSetup {
        scope,
        platform: platform.to_owned(),
        config,
    })
}
