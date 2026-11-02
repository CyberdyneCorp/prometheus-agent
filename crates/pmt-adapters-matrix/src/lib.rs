//! The first-party Matrix adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; First-party adapter transports and
//! required settings): the client-server API of `options.homeserver` with
//! the access token in the profile's secret `MATRIX_ACCESS_TOKEN` (or the
//! table's `token_secret`). Inbound is a `/sync` long poll whose cursor
//! survives reconnects; replies thread, edit (`m.replace`), react
//! (`m.annotation`) and carry media from the media repository. Rooms are
//! unencrypted here; end-to-end encryption is a later milestone.

mod api;
/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;
mod sync;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use api::{Api, segment};
use pmt_config::ProfileScope;
use pmt_config::platforms::MatrixOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, Attachment,
    ChatInfo, ChatType, Descriptor, LengthUnit, MediaKind, MediaStream, Outgoing, Reaction,
    SendError, Sent,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "matrix";
/// The secret holding the access token unless the table names another.
pub const TOKEN_SECRET: &str = "MATRIX_ACCESS_TOKEN";

/// The factory the daemon registers for `matrix`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Matrix::new(setup)?) as Arc<dyn Adapter>))
}

/// A Matrix account of one profile.
pub struct Matrix {
    scope: ProfileScope,
    secret: String,
    homeserver: Option<String>,
    allow_users: Vec<String>,
    client: reqwest::Client,
    /// Transaction ids: unique per process and start.
    txn_base: u64,
    txn: AtomicU64,
    cursor: Arc<Mutex<sync::Cursor>>,
    user_id: Mutex<Option<String>>,
    /// Reaction events the adapter added: (message, emoji) -> event id.
    reactions: Mutex<HashMap<(String, String), String>>,
    api: Mutex<Option<Api>>,
    poller: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Matrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matrix")
            .field("profile", &self.scope.name().as_str())
            .field("homeserver", &self.homeserver)
            .finish_non_exhaustive()
    }
}

fn missing_homeserver() -> AdapterError {
    AdapterError {
        retryable: false,
        code: "missing_option".to_owned(),
        message: "gateway.platforms.matrix.options.homeserver is required".to_owned(),
    }
}

impl Matrix {
    /// The adapter for `setup`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup) -> Result<Self, Error> {
        let options: MatrixOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        let network = &setup.scope.config().network;
        let policy = pmt_net::OutboundPolicy {
            force_ipv4: network.force_ipv4,
            extra_ca_file: network.extra_ca_file.as_deref().map(Into::into),
            proxy: None,
        };
        let client = pmt_net::http::client_builder(&policy)?
            // Adapters never pick up ambient proxy settings.
            .no_proxy()
            .build()
            .map_err(|e| {
                Error::new(
                    &pmt_core::error::codes::NET_CLIENT_INIT,
                    format!("cannot build the Matrix client: {e}"),
                )
            })?;
        let txn_base = jiff::Timestamp::now().as_millisecond().unsigned_abs();
        Ok(Self {
            scope: setup.scope.clone(),
            secret: setup
                .config
                .token_secret
                .as_ref()
                .map_or_else(|| TOKEN_SECRET.to_owned(), |s| s.0.clone()),
            homeserver: options
                .homeserver
                .map(|h| h.trim_end_matches('/').to_owned()),
            allow_users: setup.config.allow_users.clone(),
            client,
            txn_base,
            txn: AtomicU64::new(0),
            cursor: Arc::default(),
            user_id: Mutex::default(),
            reactions: Mutex::default(),
            api: Mutex::default(),
            poller: Mutex::default(),
        })
    }

    fn api_with(&self, token: String) -> Result<Api, AdapterError> {
        Ok(Api {
            client: self.client.clone(),
            homeserver: self.homeserver.clone().ok_or_else(missing_homeserver)?,
            token,
        })
    }

    fn api(&self) -> Result<Api, SendError> {
        if let Some(api) = self.api.lock().expect("api poisoned").clone() {
            return Ok(api);
        }
        // A one-shot send without the daemon has not connected.
        credential(&self.scope, &self.secret)
            .and_then(|token| self.api_with(token))
            .map_err(|e| SendError::Permanent(e.message))
    }

    fn txn(&self) -> String {
        let n = self.txn.fetch_add(1, Ordering::Relaxed);
        format!("pmt{}.{n}", self.txn_base)
    }

    async fn send_event(
        &self,
        room: &str,
        kind: &str,
        content: &Value,
    ) -> Result<String, SendError> {
        let path = format!(
            "/rooms/{}/send/{kind}/{}",
            segment(room),
            segment(&self.txn())
        );
        let sent = self.api()?.put(&path, content).await?;
        sent["event_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| SendError::Server("the send returned no event id".to_owned()))
    }

    async fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        let api = self.api_with(credential(&self.scope, &self.secret)?)?;
        let me = api
            .get("/account/whoami")
            .await
            .map_err(|e| sync::sync_error(&e).1)?;
        let user_id = me["user_id"].as_str().unwrap_or_default().to_owned();
        let stop = CancellationToken::new();
        let run = sync::Run {
            api: api.clone(),
            link,
            user_id: user_id.clone(),
            cursor: Arc::clone(&self.cursor),
            allow_users: self.allow_users.clone(),
            stop: stop.clone(),
        };
        run.start_cursor()
            .await
            .map_err(|e| sync::sync_error(&e).1)?;
        if let Some(old) = self.poller.lock().expect("poller poisoned").replace(stop) {
            old.cancel();
        }
        *self.user_id.lock().expect("user poisoned") = Some(user_id);
        *self.api.lock().expect("api poisoned") = Some(api);
        tokio::spawn(run.poll());
        Ok(())
    }

    async fn send_file(
        &self,
        message: &Outgoing,
        path: &Path,
        as_voice: bool,
    ) -> Result<String, SendError> {
        let (msgtype, mime) = file_kind(path, as_voice);
        let uri = self.api()?.upload(path, mime).await?;
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let mut content = json!({"msgtype": msgtype, "body": name, "url": uri,
                                 "info": {"mimetype": mime}});
        if as_voice {
            content["org.matrix.msc3245.voice"] = json!({});
        }
        relate(&mut content, message);
        self.send_event(&message.chat_id, "m.room.message", &content)
            .await
    }
}

/// The message type and MIME type of an outbound file.
fn file_kind(path: &Path, as_voice: bool) -> (&'static str, &'static str) {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        _ if as_voice => ("m.audio", "audio/ogg"),
        "png" => ("m.image", "image/png"),
        "jpg" | "jpeg" => ("m.image", "image/jpeg"),
        "gif" => ("m.image", "image/gif"),
        "webp" => ("m.image", "image/webp"),
        "ogg" | "oga" | "opus" => ("m.audio", "audio/ogg"),
        "mp3" => ("m.audio", "audio/mpeg"),
        "m4a" => ("m.audio", "audio/mp4"),
        "wav" => ("m.audio", "audio/wav"),
        "mp4" => ("m.video", "video/mp4"),
        "webm" => ("m.video", "video/webm"),
        "pdf" => ("m.file", "application/pdf"),
        _ => ("m.file", "application/octet-stream"),
    }
}

/// Puts a message in its thread and marks what it answers.
fn relate(content: &mut Value, message: &Outgoing) {
    match (&message.thread_id, &message.reply_to) {
        (Some(thread), reply) => {
            let quoted = reply.as_deref().unwrap_or(thread);
            content["m.relates_to"] = json!({
                "rel_type": "m.thread",
                "event_id": thread,
                "is_falling_back": reply.is_none(),
                "m.in_reply_to": {"event_id": quoted},
            });
        }
        (None, Some(reply)) => {
            content["m.relates_to"] = json!({"m.in_reply_to": {"event_id": reply}});
        }
        (None, None) => {}
    }
}

impl Adapter for Matrix {
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
            .map_or(32_000, |p| p.max_message_length as usize);
        Descriptor {
            length_unit: LengthUnit::Utf8Bytes,
            supports_edit: true,
            supports_delete: true,
            supports_typing: true,
            supports_reactions: true,
            supports_threads: true,
            media_in: all.clone(),
            media_out: all,
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
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            let mut first = None;
            if !message.text.is_empty() || message.attachments.is_empty() {
                let mut content = json!({"msgtype": "m.text", "body": message.text});
                relate(&mut content, &message);
                first = Some(
                    self.send_event(&message.chat_id, "m.room.message", &content)
                        .await?,
                );
            }
            for file in &message.attachments {
                let id = self
                    .send_file(&message, Path::new(&file.path), file.as_voice)
                    .await?;
                first.get_or_insert(id);
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
            let content = json!({
                "msgtype": "m.text",
                "body": format!("* {}", message.text),
                "m.new_content": {"msgtype": "m.text", "body": message.text},
                "m.relates_to": {"rel_type": "m.replace", "event_id": message_id},
            });
            self.send_event(&message.chat_id, "m.room.message", &content)
                .await
                .map(drop)
        })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let key = (reaction.message_id.clone(), reaction.emoji.clone());
            if reaction.remove {
                let Some(event) = self
                    .reactions
                    .lock()
                    .expect("reactions poisoned")
                    .remove(&key)
                else {
                    return Ok(());
                };
                let path = format!(
                    "/rooms/{}/redact/{}/{}",
                    segment(&reaction.chat_id),
                    segment(&event),
                    segment(&self.txn())
                );
                return self.api()?.put(&path, &json!({})).await.map(drop);
            }
            let content = json!({"m.relates_to": {
                "rel_type": "m.annotation",
                "event_id": reaction.message_id,
                "key": reaction.emoji,
            }});
            let event = self
                .send_event(&reaction.chat_id, "m.reaction", &content)
                .await?;
            self.reactions
                .lock()
                .expect("reactions poisoned")
                .insert(key, event);
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let path = format!(
                "/rooms/{}/redact/{}/{}",
                segment(chat_id),
                segment(message_id),
                segment(&self.txn())
            );
            self.api()?.put(&path, &json!({})).await.map(drop)
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let api = self.api()?;
            let count = sync::members(&api, &self.cursor, chat_id).await;
            let name = api
                .get(&format!("/rooms/{}/state/m.room.name", segment(chat_id)))
                .await
                .ok()
                .and_then(|n| n["name"].as_str().map(str::to_owned))
                .unwrap_or_else(|| chat_id.to_owned());
            Ok(ChatInfo {
                name,
                chat_type: if count == 2 {
                    ChatType::Dm
                } else {
                    ChatType::Group
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
            let user = self
                .user_id
                .lock()
                .expect("user poisoned")
                .clone()
                .ok_or(SendError::Unsupported)?;
            let path = format!("/rooms/{}/typing/{}", segment(chat_id), segment(&user));
            self.api()?
                .put(&path, &json!({"typing": true, "timeout": 10_000}))
                .await
                .map(drop)
        })
    }

    fn credential_identity(&self) -> Option<String> {
        credential(&self.scope, &self.secret).ok()
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move { self.api()?.download(&attachment.location).await })
    }
}
