//! The first-party Telegram adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; Adapter contract): Bot API long
//! polling with the update cursor kept across reconnects (Telegram holds
//! unconfirmed updates, so a dropped connection loses nothing), inline
//! keyboards for cards, typing, reactions, edits, and media in both
//! directions. The token is the profile's secret `TELEGRAM_BOT_TOKEN`
//! (or the table's `token_secret`), read from that profile's store only.
//!
//! In webhook mode (`options.mode = "webhook"`) updates arrive as posts on
//! the shared listener at `/p/<profile>/telegram/`, each carrying the
//! secret token registered with `setWebhook`; switching back to polling
//! deletes the webhook first. The bot's command menu and inline picker,
//! and its topics, are their own modules.

mod api;
mod commands;
/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;
mod normalize;
mod private;
mod rich;
mod text;
mod topics;
mod updates;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use api::Api;
pub use api::Endpoints;
use pmt_config::ProfileScope;
use pmt_config::platforms::{TelegramMode, TelegramOptions};
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::sign::constant_time_eq;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, Attachment,
    ChatInfo, Descriptor, HttpRequest, HttpResponse, Interactive, LengthUnit, Markup, MediaKind,
    MediaStream, MenuCommand, Outgoing, Private, Reaction, SendError, Sent, SessionHints,
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
    /// Every command and skill (the inline picker searches them all).
    commands: Arc<Mutex<Vec<MenuCommand>>>,
    topics: Arc<topics::Topics>,
    /// In webhook mode: how updates are handled and the secret token
    /// each post must carry.
    hook: Mutex<Option<(updates::Ctx, String)>>,
    /// The Bot API has no rich messages: `MarkdownV2` from then on.
    rich_unavailable: AtomicBool,
    /// The bot has no topic mode in private chats (`getMe`): multi-session
    /// DM topics fall back to one session per DM.
    dm_topics_unavailable: AtomicBool,
    /// Pressed buttons whose answer waits for a private reply.
    callbacks: Arc<private::Callbacks>,
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
    /// Multi-session DM topics need the bot's topic mode in private chats
    /// (`getMe` `has_topics_enabled`); without it they fall back to one
    /// session per DM, logged once.
    fn check_dm_topics(&self, me: &Value) {
        if !self.options.dm_topics.enabled {
            return;
        }
        let unavailable = me["has_topics_enabled"].as_bool() == Some(false);
        let before = self
            .dm_topics_unavailable
            .swap(unavailable, Ordering::AcqRel);
        if unavailable && !before {
            tracing::warn!(
                platform = PLATFORM,
                "options.dm_topics.enabled, but the bot has no topic mode in private chats \
                 (BotFather > Bot Settings > Threaded Mode); DMs stay one session each"
            );
        }
    }

    /// The adapter for `setup`, talking to `endpoints`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup, endpoints: Endpoints) -> Result<Self, Error> {
        let options: TelegramOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        // Direct unless a proxy is configured or the environment trusted.
        let (builder, _) = pmt_gateway::messaging::proxy::client_builder(setup)?;
        let client = builder.build().map_err(|e| {
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
            endpoints,
            client,
            offset: Arc::default(),
            api: Mutex::default(),
            poller: Mutex::default(),
            commands: Arc::default(),
            topics: Arc::new(topics::Topics::load(
                setup.scope.path("platforms/telegram/topics.json"),
                &options,
            )),
            hook: Mutex::default(),
            callbacks: Arc::default(),
            rich_unavailable: AtomicBool::new(false),
            dm_topics_unavailable: AtomicBool::new(false),
            options,
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
pub(crate) fn chat(id: &str) -> Value {
    id.parse::<i64>().map_or_else(|_| json!(id), |n| json!(n))
}

fn number(id: &str) -> Result<i64, SendError> {
    id.parse()
        .map_err(|_| SendError::Permanent(format!("'{id}' is not a Telegram message id")))
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

impl Telegram {
    /// Whether replies go out as rich messages.
    fn rich(&self) -> bool {
        self.options.rich_messages && !self.rich_unavailable.load(Ordering::Relaxed)
    }

    /// What the host renders replies in: `MarkdownV2`, or the raw
    /// Markdown of rich messages.
    fn markup(&self) -> Markup {
        if self.rich() {
            Markup::Markdown
        } else {
            Markup::Platform
        }
    }

    /// Registers the webhook with its secret token; updates then arrive on
    /// the shared listener.
    async fn start_webhook(&self, ctx: updates::Ctx) -> Result<(), AdapterError> {
        let missing = |what: &str| AdapterError {
            retryable: false,
            code: "missing_option".to_owned(),
            message: format!(
                "gateway.platforms.telegram.options.{what} is required in webhook mode"
            ),
        };
        let name = self
            .options
            .webhook_secret_secret
            .as_ref()
            .ok_or_else(|| missing("webhook_secret_secret"))?;
        let url = self
            .options
            .webhook_public_url
            .clone()
            .ok_or_else(|| missing("webhook_public_url"))?;
        let secret = credential(&self.scope, &name.0)?;
        let body = json!({"url": url, "secret_token": secret,
                          "allowed_updates": updates::ALLOWED});
        ctx.api
            .call("setWebhook", &body)
            .await
            .map_err(|e| connect_error(&e))?;
        *self.hook.lock().expect("hook poisoned") = Some((ctx, secret));
        Ok(())
    }

    /// Deletes any webhook (Telegram refuses `getUpdates` while one is
    /// set), then polls.
    async fn start_polling(&self, ctx: updates::Ctx) -> Result<(), AdapterError> {
        ctx.api
            .call("deleteWebhook", &json!({"drop_pending_updates": false}))
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
        let timeout = self.options.poll_timeout_secs;
        tokio::spawn(updates::poll(ctx, Arc::clone(&self.offset), timeout, stop));
        Ok(())
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
        let limit = if self.rich() {
            rich::LIMIT
        } else {
            text::LIMIT
        };
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
            private_replies: true,
            dm_topics: self.options.dm_topics.enabled
                && !self.dm_topics_unavailable.load(Ordering::Acquire),
            markup: self.markup(),
            binds_http: self.options.mode == TelegramMode::Webhook,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(async move {
            let token = credential(&self.scope, &self.secret)?;
            let api = self.api_with(token);
            let me = api
                .call("getMe", &json!({}))
                .await
                .map_err(|e| connect_error(&e))?;
            *self.api.lock().expect("api poisoned") = Some(api.clone());
            self.check_dm_topics(&me);
            self.topics.create_missing(&api).await;
            let ctx = updates::Ctx {
                api: api.clone(),
                link,
                commands: Arc::clone(&self.commands),
                topics: Arc::clone(&self.topics),
                ignore_root_dm: self.options.ignore_root_dm,
                me: normalize::Me::of(&me),
                callbacks: Arc::clone(&self.callbacks),
            };
            match self.options.mode {
                TelegramMode::Webhook => self.start_webhook(ctx).await,
                TelegramMode::Polling => self.start_polling(ctx).await,
            }
        })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.poller.lock().expect("poller poisoned").take() {
            stop.cancel();
        }
        self.hook.lock().expect("hook poisoned").take();
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            let api = self.api()?;
            let mut first = None;
            if !message.text.is_empty() || message.attachments.is_empty() {
                first = self.send_text(&api, &message).await?;
            }
            for file in &message.attachments {
                let path = Path::new(&file.path);
                let (method, field) = upload_kind(path, file.as_document, file.as_voice);
                let mut fields = vec![("chat_id", message.chat_id.clone())];
                if let Some(thread) = &message.thread_id {
                    fields.push(("message_thread_id", thread.clone()));
                }
                if message.notify == Some(false) {
                    fields.push(("disable_notification", "true".to_owned()));
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
            let api = self.api()?;
            self.edit_text(&api, number(message_id)?, message).await
        })
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let body = json!({"chat_id": chat(chat_id), "message_id": number(message_id)?});
            self.api()?.call("deleteMessage", &body).await.map(drop)
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

    fn verify_http(&self, request: &HttpRequest) -> bool {
        let hook = self.hook.lock().expect("hook poisoned");
        let Some((_, secret)) = hook.as_ref() else {
            return false;
        };
        request
            .header("x-telegram-bot-api-secret-token")
            .is_some_and(|token| constant_time_eq(token.as_bytes(), secret.as_bytes()))
    }

    fn handle_http<'a>(&'a self, request: &'a HttpRequest) -> AdapterFuture<'a, HttpResponse> {
        Box::pin(async move {
            let ctx = self
                .hook
                .lock()
                .expect("hook poisoned")
                .as_ref()
                .map(|(c, _)| c.clone());
            let Some(ctx) = ctx else {
                return HttpResponse::status(503);
            };
            if request.method != "POST" {
                return HttpResponse::status(405);
            }
            let Ok(update) = serde_json::from_slice::<Value>(&request.body) else {
                return HttpResponse::status(400);
            };
            updates::handle(&ctx, &update).await;
            HttpResponse::status(200)
        })
    }

    fn register_commands<'a>(
        &'a self,
        commands: &'a [MenuCommand],
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            *self.commands.lock().expect("commands poisoned") = commands::sanitized(commands);
            commands::register(&self.api()?, &self.options, commands).await
        })
    }

    fn open_thread<'a>(
        &'a self,
        chat_id: &'a str,
        title: &'a str,
    ) -> AdapterFuture<'a, Result<String, SendError>> {
        Box::pin(async move {
            let thread = topics::open(&self.api()?, chat_id, title).await?;
            if let Ok(id) = thread.parse() {
                self.topics.record(chat_id, title, id);
            }
            Ok(thread)
        })
    }

    fn rename_thread<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: &'a str,
        title: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move { topics::rename(&self.api()?, chat_id, thread_id, title).await })
    }

    fn reply_privately<'a>(&'a self, to: &'a Private) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move { private::reply(&self.api()?, &self.callbacks, to).await })
    }

    fn session_hints(&self, chat_id: &str, thread_id: Option<&str>) -> SessionHints {
        let skill = thread_id.and_then(|thread| self.topics.skill(chat_id, thread));
        SessionHints {
            skills: skill.into_iter().collect(),
            toolsets: None,
        }
    }
}
