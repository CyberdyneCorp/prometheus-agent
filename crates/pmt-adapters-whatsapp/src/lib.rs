//! The first-party `WhatsApp` adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; First-party adapter transports and
//! required settings): the `WhatsApp` Web multi-device protocol through a
//! bridge runtime on loopback HTTP — the managed one the adapter starts,
//! or one already running at `options.bridge_url`. Each profile links its
//! own device; one person is one sender whether `WhatsApp` names them by
//! phone number or LID. Choices are numbered text (native polls come with
//! the interactive features).

mod bridge;
/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;
mod normalize;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bridge::Bridge;
use normalize::Aliases;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use pmt_config::ProfileScope;
use pmt_config::platforms::WhatsappOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, AdapterState,
    Attachment, ChatInfo, ChatType, Descriptor, Markup, MediaKind, MediaStream, Outgoing,
    SendError, Sent,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "whatsapp";
/// How often the bridge's queue is fetched.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a connect waits for the bridge to reach `WhatsApp`.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The factory the daemon registers for `whatsapp`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Whatsapp::new(setup)?) as Arc<dyn Adapter>))
}

/// A linked `WhatsApp` device of one profile.
pub struct Whatsapp {
    scope: ProfileScope,
    bridge_url: Option<String>,
    client: reqwest::Client,
    aliases: Arc<Aliases>,
    bridge: Mutex<Option<Bridge>>,
    /// The managed bridge process, when the adapter started one.
    child: Mutex<Option<tokio::process::Child>>,
    poller: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Whatsapp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Whatsapp")
            .field("profile", &self.scope.name().as_str())
            .field("bridge_url", &self.bridge_url)
            .finish_non_exhaustive()
    }
}

fn unreachable(url: &str, why: &str) -> AdapterError {
    AdapterError {
        retryable: true,
        code: "whatsapp_bridge_unreachable".to_owned(),
        message: format!("the WhatsApp bridge at {url} is unreachable: {why}"),
    }
}

impl Whatsapp {
    /// The adapter for `setup`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup) -> Result<Self, Error> {
        let options: WhatsappOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        // The bridge is a loopback service: never through a proxy.
        let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())?
            .no_proxy()
            .build()
            .map_err(|e| {
                Error::new(
                    &pmt_core::error::codes::NET_CLIENT_INIT,
                    format!("cannot build the WhatsApp bridge client: {e}"),
                )
            })?;
        let aliases = Aliases::load(setup.scope.path("platforms/whatsapp/aliases.json"));
        Ok(Self {
            scope: setup.scope.clone(),
            bridge_url: options
                .bridge_url
                .map(|u| u.trim_end_matches('/').to_owned()),
            client,
            aliases: Arc::new(aliases),
            bridge: Mutex::default(),
            child: Mutex::default(),
            poller: Mutex::default(),
        })
    }

    fn profile(&self) -> &str {
        self.scope.name().as_str()
    }

    fn bridge(&self) -> Result<Bridge, SendError> {
        if let Some(bridge) = self.bridge.lock().expect("bridge poisoned").clone() {
            return Ok(bridge);
        }
        match &self.bridge_url {
            Some(url) => Ok(Bridge {
                client: self.client.clone(),
                url: url.clone(),
            }),
            None => Err(SendError::Network(
                "the WhatsApp bridge runs only while the daemon is connected".to_owned(),
            )),
        }
    }

    /// The bridge to talk to: the configured one, the running managed
    /// one, or a newly started managed one.
    fn start_bridge(&self) -> Result<Bridge, AdapterError> {
        if let Some(url) = &self.bridge_url {
            return Ok(Bridge {
                client: self.client.clone(),
                url: url.clone(),
            });
        }
        let mut child = self.child.lock().expect("child poisoned");
        let running = child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if running && let Some(bridge) = self.bridge.lock().expect("bridge poisoned").clone() {
            return Ok(bridge);
        }
        let (process, url) = bridge::launch(self.scope.home(), self.scope.dir(), self.profile())?;
        *child = Some(process);
        Ok(Bridge {
            client: self.client.clone(),
            url,
        })
    }

    /// Waits until the bridge reaches `WhatsApp`.
    async fn ready(&self, bridge: &Bridge) -> Result<(), AdapterError> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            let error = match bridge.get("/health").await {
                Ok(health) => match bridge::health_error(&health, self.profile()) {
                    None => return Ok(()),
                    Some(err) if !err.retryable => return Err(err),
                    Some(err) => err,
                },
                Err(err) => unreachable(&bridge.url, &err.describe()),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(error);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        let bridge = self.start_bridge()?;
        self.ready(&bridge).await?;
        let stop = CancellationToken::new();
        if let Some(old) = self
            .poller
            .lock()
            .expect("poller poisoned")
            .replace(stop.clone())
        {
            old.cancel();
        }
        *self.bridge.lock().expect("bridge poisoned") = Some(bridge.clone());
        let poll = Poll {
            bridge,
            link,
            aliases: Arc::clone(&self.aliases),
            profile: self.profile().to_owned(),
            stop,
        };
        tokio::spawn(poll.run());
        Ok(())
    }

    async fn send_file(
        &self,
        bridge: &Bridge,
        chat: &str,
        path: &str,
        kind: &str,
    ) -> Result<Option<String>, SendError> {
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let body = json!({"chatId": chat, "filePath": path, "mediaType": kind, "fileName": name});
        let sent = bridge.post("/send-media", &body).await?;
        Ok(sent["messageId"].as_str().map(str::to_owned))
    }
}

/// The bridge's media type of an outbound file.
fn media_type(path: &str, as_document: bool, as_voice: bool) -> &'static str {
    if as_voice {
        return "audio";
    }
    if as_document {
        return "document";
    }
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "gif" => "image",
        "mp4" | "mov" | "3gp" => "video",
        "ogg" | "opus" | "mp3" | "m4a" | "wav" | "aac" => "audio",
        _ => "document",
    }
}

/// The queue poller of one connection.
struct Poll {
    bridge: Bridge,
    link: Arc<dyn AdapterLink>,
    aliases: Arc<Aliases>,
    profile: String,
    stop: CancellationToken,
}

impl Poll {
    async fn run(self) {
        loop {
            let fetched = tokio::select! {
                () = self.stop.cancelled() => return,
                fetched = self.bridge.get("/messages") => fetched,
            };
            let events = match fetched {
                Ok(events) => events,
                Err(err) => return self.lost(&err).await,
            };
            for raw in events.as_array().into_iter().flatten() {
                if let Some(inbound) = normalize::event(raw, &self.aliases) {
                    self.link.submit(inbound).await;
                }
            }
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    /// Reports the lost bridge: fatal when `WhatsApp` logged the device out.
    async fn lost(&self, err: &SendError) {
        let logged_out = match self.bridge.get("/health").await {
            Ok(health) => bridge::health_error(&health, &self.profile).filter(|e| !e.retryable),
            Err(_) => None,
        };
        let (state, error) = match logged_out {
            Some(error) => (AdapterState::Fatal, error),
            None => (
                AdapterState::Retrying,
                unreachable(&self.bridge.url, &err.describe()),
            ),
        };
        tracing::warn!(platform = PLATFORM, "{}", error.message);
        self.link.report(state, Some(error));
    }
}

impl Adapter for Whatsapp {
    fn describe(&self) -> Descriptor {
        let all: BTreeSet<MediaKind> = [
            MediaKind::Image,
            MediaKind::Audio,
            MediaKind::Voice,
            MediaKind::Video,
            MediaKind::Document,
        ]
        .into_iter()
        .collect();
        let limit = pmt_config::platforms::first_party(PLATFORM)
            .map_or(4096, |p| p.max_message_length as usize);
        // The bridge sends every audio file as a voice note (it converts
        // to Ogg Opus and sets `ptt`), so a plain audio file cannot go out.
        let mut out = all.clone();
        out.remove(&MediaKind::Audio);
        Descriptor {
            markup: Markup::Platform,
            supports_edit: true,
            supports_typing: true,
            media_in: all,
            media_out: out,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(self.connect_with(link))
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.poller.lock().expect("poller poisoned").take() {
            stop.cancel();
        }
        // Dropping the managed bridge kills it.
        self.child.lock().expect("child poisoned").take();
        self.bridge.lock().expect("bridge poisoned").take();
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            let bridge = self.bridge()?;
            let mut first = None;
            if !message.text.is_empty() || message.attachments.is_empty() {
                let mut body = json!({"chatId": message.chat_id, "message": message.text});
                if let Some(reply) = &message.reply_to {
                    body["replyTo"] = json!(reply);
                }
                let sent = bridge.post("/send", &body).await?;
                first = sent["messageId"].as_str().map(str::to_owned);
            }
            for file in &message.attachments {
                let kind = media_type(&file.path, file.as_document, file.as_voice);
                let id = self
                    .send_file(&bridge, &message.chat_id, &file.path, kind)
                    .await?;
                first = first.or(id);
            }
            Ok(Sent { message_id: first })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let body = json!({"chatId": message.chat_id, "messageId": message_id,
                              "message": message.text});
            self.bridge()?.post("/edit", &body).await.map(drop)
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let path = format!("/chat/{}", utf8_percent_encode(chat_id, NON_ALPHANUMERIC));
            let found = self.bridge()?.get(&path).await?;
            Ok(ChatInfo {
                name: found["name"].as_str().unwrap_or(chat_id).to_owned(),
                chat_type: if found["isGroup"].as_bool() == Some(true) {
                    ChatType::Group
                } else {
                    ChatType::Dm
                },
            })
        })
    }

    fn send_typing<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let done = self
                .bridge()?
                .post("/typing", &json!({"chatId": chat_id}))
                .await?;
            if done["success"].as_bool() == Some(false) {
                return Err(SendError::Server(
                    "the bridge could not show typing".to_owned(),
                ));
            }
            Ok(())
        })
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move {
            // The bridge downloads media into its own cache on this machine.
            let bytes = tokio::fs::read(&attachment.location)
                .await
                .map_err(|e| SendError::Permanent(format!("{}: {e}", attachment.location)))?;
            Ok(MediaStream {
                declared_len: Some(bytes.len() as u64),
                chunks: Box::pin(futures_util::stream::iter([Ok::<_, String>(bytes)])),
            })
        })
    }
}

impl Drop for Whatsapp {
    fn drop(&mut self) {
        if let Some(stop) = self.poller.lock().map(|mut p| p.take()).ok().flatten() {
            stop.cancel();
        }
    }
}
