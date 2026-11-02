//! The first-party Telegram adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; Adapter contract): Bot API long
//! polling with the update cursor kept across reconnects (Telegram holds
//! unconfirmed updates, so a dropped connection loses nothing), inline
//! keyboards for cards, typing, reactions, edits, and media in both
//! directions. The token is the profile's secret `TELEGRAM_BOT_TOKEN`
//! (or the table's `token_secret`), read from that profile's store only.

mod api;
mod normalize;

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::Api;
pub use api::Endpoints;
use pmt_config::ProfileScope;
use pmt_config::platforms::TelegramOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, AdapterState,
    Attachment, ChatInfo, Descriptor, Interactive, LengthUnit, MediaKind, MediaStream, Outgoing,
    Reaction, SendError, Sent,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "telegram";
/// The secret holding the bot token unless the table names another.
pub const TOKEN_SECRET: &str = "TELEGRAM_BOT_TOKEN";

/// The factory the daemon registers for `telegram`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Telegram::new(setup, Endpoints::default())?) as Arc<dyn Adapter>))
}

/// A Telegram bot of one profile.
pub struct Telegram {
    scope: ProfileScope,
    secret: String,
    options: TelegramOptions,
    endpoints: Endpoints,
    client: reqwest::Client,
    /// The next update id to ask for (kept across reconnects).
    offset: Arc<AtomicI64>,
    api: Mutex<Option<Api>>,
    poller: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("profile", &self.scope.name().as_str())
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl Telegram {
    /// The adapter for `setup`, talking to `endpoints`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup, endpoints: Endpoints) -> Result<Self, Error> {
        let options: TelegramOptions =
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
                    format!("cannot build the Telegram client: {e}"),
                )
            })?;
        let secret = setup
            .config
            .token_secret
            .as_ref()
            .map_or_else(|| TOKEN_SECRET.to_owned(), |s| s.0.clone());
        Ok(Self {
            scope: setup.scope.clone(),
            secret,
            options,
            endpoints,
            client,
            offset: Arc::default(),
            api: Mutex::default(),
            poller: Mutex::default(),
        })
    }

    fn api(&self) -> Result<Api, SendError> {
        if let Some(api) = self.api.lock().expect("api poisoned").clone() {
            return Ok(api);
        }
        // A one-shot send without the daemon has not connected.
        let token =
            credential(&self.scope, &self.secret).map_err(|e| SendError::Permanent(e.message))?;
        Ok(self.api_with(token))
    }

    fn api_with(&self, token: String) -> Api {
        Api {
            client: self.client.clone(),
            endpoints: self.endpoints.clone(),
            token,
        }
    }
}

/// A chat or thread id as the Bot API takes it (numbers stay numbers).
fn chat(id: &str) -> Value {
    id.parse::<i64>().map_or_else(|_| json!(id), |n| json!(n))
}

fn number(id: &str) -> Result<i64, SendError> {
    id.parse()
        .map_err(|_| SendError::Permanent(format!("'{id}' is not a Telegram message id")))
}

fn keyboard(message: &Outgoing) -> Value {
    let rows: Vec<Value> = message
        .buttons
        .iter()
        .map(|b| json!([{"text": b.label, "callback_data": b.data}]))
        .collect();
    json!({"inline_keyboard": rows})
}

/// The `sendMessage` body of a message: its thread, the message it
/// answers and its buttons.
fn text_body(message: &Outgoing) -> Value {
    let mut body = json!({"chat_id": chat(&message.chat_id), "text": message.text});
    if let Some(thread) = &message.thread_id {
        body["message_thread_id"] = chat(thread);
    }
    if let Some(reply) = &message.reply_to {
        body["reply_parameters"] =
            json!({"message_id": chat(reply), "allow_sending_without_reply": true});
    }
    if !message.buttons.is_empty() {
        body["reply_markup"] = keyboard(message);
    }
    body
}

/// The upload method and field of an outbound file.
fn upload_kind(path: &Path, as_document: bool, as_voice: bool) -> (&'static str, &'static str) {
    if as_voice {
        return ("sendVoice", "voice");
    }
    if as_document {
        return ("sendDocument", "document");
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" => ("sendPhoto", "photo"),
        "ogg" | "oga" | "opus" => ("sendVoice", "voice"),
        "mp3" | "m4a" | "wav" | "flac" => ("sendAudio", "audio"),
        "mp4" | "mov" | "webm" => ("sendVideo", "video"),
        _ => ("sendDocument", "document"),
    }
}

/// Polls `getUpdates` until stopped or the connection fails; the cursor
/// moves past an update only after the gateway took it.
async fn poll(
    api: Api,
    link: Arc<dyn AdapterLink>,
    offset: Arc<AtomicI64>,
    timeout_secs: u64,
    stop: CancellationToken,
) {
    loop {
        let body = json!({
            "offset": offset.load(Ordering::Acquire),
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        let wait = Duration::from_secs(timeout_secs + 10);
        let polled = tokio::select! {
            () = stop.cancelled() => return,
            polled = api.call_within("getUpdates", &body, wait) => polled,
        };
        let updates = match polled {
            Ok(updates) => updates,
            Err(err) => return lost(link.as_ref(), &err),
        };
        for raw in updates.as_array().into_iter().flatten() {
            if let Some(update) = normalize::update(raw) {
                if let Some(query) = &update.callback_query {
                    let _ = api
                        .call("answerCallbackQuery", &json!({"callback_query_id": query}))
                        .await;
                }
                link.submit(update.inbound).await;
            }
            if let Some(id) = raw["update_id"].as_i64() {
                offset.store(id + 1, Ordering::Release);
            }
        }
    }
}

/// Reports a failed poll: retried by the gateway unless the token is bad.
fn lost(link: &dyn AdapterLink, err: &SendError) {
    let (state, code, retryable) = match err {
        SendError::Permanent(why) if why.starts_with("401") => {
            (AdapterState::Fatal, "unauthorized", false)
        }
        SendError::Permanent(why) if why.starts_with("409") => {
            (AdapterState::Retrying, "telegram_conflict", true)
        }
        _ => (AdapterState::Retrying, "telegram_unreachable", true),
    };
    tracing::warn!(platform = PLATFORM, "polling stopped: {}", err.describe());
    link.report(
        state,
        Some(AdapterError {
            retryable,
            code: code.to_owned(),
            message: err.describe(),
        }),
    );
}

fn connect_error(err: &SendError) -> AdapterError {
    let unauthorized = matches!(err, SendError::Permanent(why) if why.starts_with("401"));
    AdapterError {
        retryable: !unauthorized,
        code: if unauthorized {
            "unauthorized"
        } else {
            "telegram_unreachable"
        }
        .to_owned(),
        message: format!("Telegram: {}", err.describe()),
    }
}

impl Adapter for Telegram {
    fn describe(&self) -> Descriptor {
        let all: std::collections::BTreeSet<MediaKind> = [
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
        Descriptor {
            length_unit: LengthUnit::Utf16,
            supports_edit: true,
            supports_delete: true,
            supports_typing: true,
            supports_reactions: true,
            supports_threads: true,
            interactive: Interactive::Native,
            media_in: all.clone(),
            media_out: all,
            stateless_send: true,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(async move {
            let token = credential(&self.scope, &self.secret)?;
            let api = self.api_with(token);
            api.call("getMe", &json!({}))
                .await
                .map_err(|e| connect_error(&e))?;
            let stop = CancellationToken::new();
            if let Some(old) = self
                .poller
                .lock()
                .expect("poller poisoned")
                .replace(stop.clone())
            {
                old.cancel();
            }
            *self.api.lock().expect("api poisoned") = Some(api.clone());
            let timeout = self.options.poll_timeout_secs;
            tokio::spawn(poll(api, link, Arc::clone(&self.offset), timeout, stop));
            Ok(())
        })
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
            let api = self.api()?;
            let mut first = None;
            if !message.text.is_empty() || message.attachments.is_empty() {
                let sent = api.call("sendMessage", &text_body(&message)).await?;
                first = normalize::id(&sent["message_id"]);
            }
            for file in &message.attachments {
                let path = Path::new(&file.path);
                let (method, field) = upload_kind(path, file.as_document, file.as_voice);
                let mut fields = vec![("chat_id", message.chat_id.clone())];
                if let Some(thread) = &message.thread_id {
                    fields.push(("message_thread_id", thread.clone()));
                }
                let sent = api.upload(method, &fields, field, path).await?;
                first = first.or_else(|| normalize::id(&sent["message_id"]));
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
            let body = json!({
                "chat_id": chat(&message.chat_id),
                "message_id": number(message_id)?,
                "text": message.text,
                "reply_markup": keyboard(message),
            });
            self.api()?.call("editMessageText", &body).await.map(drop)
        })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let emoji = if reaction.remove {
                json!([])
            } else {
                json!([{"type": "emoji", "emoji": reaction.emoji}])
            };
            let body = json!({
                "chat_id": chat(&reaction.chat_id),
                "message_id": number(&reaction.message_id)?,
                "reaction": emoji,
            });
            self.api()?
                .call("setMessageReaction", &body)
                .await
                .map(drop)
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let found = self
                .api()?
                .call("getChat", &json!({"chat_id": chat(chat_id)}))
                .await?;
            let name = found["title"]
                .as_str()
                .or_else(|| found["first_name"].as_str())
                .or_else(|| found["username"].as_str())
                .unwrap_or(chat_id)
                .to_owned();
            Ok(ChatInfo {
                name,
                chat_type: normalize::chat_type(found["type"].as_str().unwrap_or_default()),
            })
        })
    }

    fn send_typing<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let mut body = json!({"chat_id": chat(chat_id), "action": "typing"});
            if let Some(thread) = thread_id {
                body["message_thread_id"] = chat(thread);
            }
            self.api()?.call("sendChatAction", &body).await.map(drop)
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
