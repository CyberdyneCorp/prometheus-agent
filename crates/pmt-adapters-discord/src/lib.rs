//! The first-party Discord adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; Adapter contract): the Gateway
//! WebSocket for inbound events (resumed after a dropped connection, so
//! Discord replays what the bot missed), REST for sends, edits, typing,
//! reactions and button components. The token is the profile's secret
//! `DISCORD_BOT_TOKEN` (or the table's `token_secret`), read from that
//! profile's store only.

mod api;
mod gateway;
pub mod mock;
mod normalize;
mod voice;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use api::Endpoints;
use api::{Api, Body};
use pmt_config::ProfileScope;
use pmt_config::platforms::DiscordOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::multipart::Form;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, Attachment,
    ChatInfo, ChatType, Descriptor, Interactive, Markup, MediaKind, MediaStream, Outgoing,
    Reaction, SendError, Sent,
};
use reqwest::Method;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "discord";
/// The secret holding the bot token unless the table names another.
pub const TOKEN_SECRET: &str = "DISCORD_BOT_TOKEN";

/// The factory the daemon registers for `discord`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Discord::new(setup, Endpoints::default())?) as Arc<dyn Adapter>))
}

/// A Discord bot of one profile.
pub struct Discord {
    scope: ProfileScope,
    secret: String,
    options: DiscordOptions,
    endpoints: Endpoints,
    client: reqwest::Client,
    extra_ca: Option<PathBuf>,
    session: Arc<Mutex<gateway::Session>>,
    api: Mutex<Option<Api>>,
    connection: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord")
            .field("profile", &self.scope.name().as_str())
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

fn unreachable(err: &SendError) -> AdapterError {
    let unauthorized = matches!(err, SendError::Permanent(why) if why.starts_with("401"));
    AdapterError {
        retryable: !unauthorized,
        code: if unauthorized {
            "unauthorized"
        } else {
            "discord_unreachable"
        }
        .to_owned(),
        message: format!("Discord: {}", err.describe()),
    }
}

impl Discord {
    /// The adapter for `setup`, talking to `endpoints`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup, endpoints: Endpoints) -> Result<Self, Error> {
        let options: DiscordOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        let network = &setup.scope.config().network;
        let extra_ca = network.extra_ca_file.as_deref().map(PathBuf::from);
        let policy = pmt_net::OutboundPolicy {
            force_ipv4: network.force_ipv4,
            extra_ca_file: extra_ca.clone(),
            proxy: None,
        };
        let client = pmt_net::http::client_builder(&policy)?
            // Adapters never pick up ambient proxy settings.
            .no_proxy()
            .build()
            .map_err(|e| {
                Error::new(
                    &pmt_core::error::codes::NET_CLIENT_INIT,
                    format!("cannot build the Discord client: {e}"),
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
            extra_ca,
            session: Arc::default(),
            api: Mutex::default(),
            connection: Mutex::default(),
        })
    }

    fn api_with(&self, token: String) -> Api {
        Api {
            client: self.client.clone(),
            endpoints: self.endpoints.clone(),
            token,
        }
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

    /// The Gateway URL: the session's resume URL, else Discord's answer.
    async fn gateway_url(&self, api: &Api) -> Result<String, AdapterError> {
        let resume = {
            let session = self.session.lock().expect("session poisoned");
            session.id.as_ref().and(session.resume_url.clone())
        };
        if let Some(url) = resume {
            return Ok(url);
        }
        let found = api
            .call(Method::GET, "/gateway/bot", Body::Empty)
            .await
            .map_err(|e| unreachable(&e))?;
        found["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreachable(&SendError::Server("no gateway URL".into())))
    }
}

/// The channel a message goes to (a thread is its own channel).
fn channel(message: &Outgoing) -> &str {
    message.thread_id.as_deref().unwrap_or(&message.chat_id)
}

fn components(message: &Outgoing) -> Value {
    let rows: Vec<Value> = message
        .buttons
        .chunks(5)
        .map(|row| {
            let buttons: Vec<Value> = row
                .iter()
                .map(|b| json!({"type": 2, "style": 2, "label": b.label, "custom_id": b.data}))
                .collect();
            json!({"type": 1, "components": buttons})
        })
        .collect();
    json!(rows)
}

fn payload(message: &Outgoing) -> Value {
    let mut body = json!({
        "content": message.text,
        "components": components(message),
        "allowed_mentions": {"parse": []},
    });
    if let Some(reply) = &message.reply_to {
        body["message_reference"] = json!({"message_id": reply, "fail_if_not_exists": false});
    }
    body
}

/// A message with files, as Discord's multipart form.
fn with_files(message: &Outgoing) -> Result<Body, SendError> {
    let mut form = Form::new();
    let mut attachments = Vec::new();
    let mut files = Vec::new();
    for (i, file) in message.attachments.iter().enumerate() {
        let path = Path::new(&file.path);
        let bytes = std::fs::read(path)
            .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
        let name = path
            .file_name()
            .map_or_else(|| format!("file{i}"), |n| n.to_string_lossy().into_owned());
        attachments.push(json!({"id": i, "filename": name}));
        files.push((format!("files[{i}]"), name, bytes));
    }
    let mut body = payload(message);
    body["attachments"] = json!(attachments);
    form.text("payload_json", &body.to_string());
    for (field, name, bytes) in &files {
        form.file(field, name, bytes);
    }
    let kind = form.content_type();
    Ok(Body::Form(kind, form.finish()))
}

fn emoji(reaction: &Reaction) -> String {
    percent_encoding::utf8_percent_encode(&reaction.emoji, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
}

impl Adapter for Discord {
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
            .map_or(2000, |p| p.max_message_length as usize);
        Descriptor {
            supports_edit: true,
            supports_delete: true,
            supports_typing: true,
            supports_reactions: true,
            supports_threads: true,
            interactive: Interactive::Native,
            media_in: all.clone(),
            media_out: all,
            markup: Markup::Markdown,
            stateless_send: true,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(async move {
            let token = credential(&self.scope, &self.secret)?;
            let api = self.api_with(token);
            let me = api
                .call(Method::GET, "/users/@me", Body::Empty)
                .await
                .map_err(|e| unreachable(&e))?;
            if let Some(id) = me["id"].as_str() {
                id.clone_into(&mut self.session.lock().expect("session poisoned").bot_id);
            }
            let url = self.gateway_url(&api).await?;
            let stop = CancellationToken::new();
            if let Some(old) = self
                .connection
                .lock()
                .expect("connection poisoned")
                .replace(stop.clone())
            {
                old.cancel();
            }
            *self.api.lock().expect("api poisoned") = Some(api.clone());
            let run = gateway::Run {
                api,
                link,
                session: Arc::clone(&self.session),
                require_mention: self.options.require_mention,
                extra_ca: self.extra_ca.clone(),
                stop,
            };
            gateway::open(run, &url).await
        })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.connection.lock().expect("connection poisoned").take() {
            stop.cancel();
        }
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            // A voice note is a message of its own (no text, one clip).
            let (voices, files) = message
                .attachments
                .iter()
                .cloned()
                .partition(|f| f.as_voice);
            let rest = Outgoing {
                attachments: files,
                ..message.clone()
            };
            let mut bodies = Vec::new();
            if !rest.text.is_empty() || !rest.attachments.is_empty() || voices.is_empty() {
                bodies.push(if rest.attachments.is_empty() {
                    Body::Json(payload(&rest))
                } else {
                    with_files(&rest)?
                });
            }
            for clip in &voices {
                bodies.push(voice::message(Path::new(&clip.path))?);
            }
            let path = format!("/channels/{}/messages", channel(&message));
            let api = self.api()?;
            let mut first = None;
            for body in bodies {
                let sent = api.call(Method::POST, &path, body).await?;
                first = first.or_else(|| sent["id"].as_str().map(str::to_owned));
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
            let path = format!("/channels/{}/messages/{message_id}", channel(message));
            let body =
                Body::Json(json!({"content": message.text, "components": components(message)}));
            self.api()?.call(Method::PATCH, &path, body).await.map(drop)
        })
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let path = format!(
                "/channels/{}/messages/{message_id}",
                thread_id.unwrap_or(chat_id)
            );
            self.api()?
                .call(Method::DELETE, &path, Body::Empty)
                .await
                .map(drop)
        })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let path = format!(
                "/channels/{}/messages/{}/reactions/{}/@me",
                reaction.chat_id,
                reaction.message_id,
                emoji(reaction)
            );
            let method = if reaction.remove {
                Method::DELETE
            } else {
                Method::PUT
            };
            self.api()?.call(method, &path, Body::Empty).await.map(drop)
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let path = format!("/channels/{chat_id}");
            let found = self.api()?.call(Method::GET, &path, Body::Empty).await?;
            let chat_type = match found["type"].as_u64() {
                Some(1) => ChatType::Dm,
                Some(3) => ChatType::Group,
                Some(10..=12) => ChatType::Thread,
                _ => ChatType::Channel,
            };
            let name = found["name"]
                .as_str()
                .or_else(|| found["recipients"][0]["username"].as_str())
                .unwrap_or(chat_id)
                .to_owned();
            Ok(ChatInfo { name, chat_type })
        })
    }

    fn send_typing<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let path = format!("/channels/{}/typing", thread_id.unwrap_or(chat_id));
            self.api()?
                .call(Method::POST, &path, Body::Empty)
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
