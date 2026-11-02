//! The first-party Slack adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; First-party adapter transports and
//! required settings): Socket Mode for events (secret `SLACK_APP_TOKEN`,
//! or `options.app_token_secret`) and the Web API for everything sent
//! (secret `SLACK_BOT_TOKEN`, or the table's `token_secret`), both read
//! from the owning profile's store only. Cards are Block Kit buttons whose
//! value is the host's signed callback data; edits are `chat.update`;
//! files go up through Slack's external upload flow.

mod api;
/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;
mod normalize;
mod socket;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use api::Api;
pub use api::Endpoints;
use pmt_config::ProfileScope;
use pmt_config::platforms::SlackOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, Attachment,
    ChatInfo, ChatType, Descriptor, Interactive, Markup, MediaKind, MediaStream, Outgoing,
    Reaction, SendError, Sent,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "slack";
/// The secret holding the bot token unless the table names another.
pub const TOKEN_SECRET: &str = "SLACK_BOT_TOKEN";
/// The secret holding the app-level token unless the options name another.
pub const APP_TOKEN_SECRET: &str = "SLACK_APP_TOKEN";
/// Characters of one Block Kit section's text.
const SECTION_CHARS: usize = 3000;

/// The factory the daemon registers for `slack`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Slack::new(setup, Endpoints::default())?) as Arc<dyn Adapter>))
}

/// A Slack app of one profile.
pub struct Slack {
    scope: ProfileScope,
    bot_secret: String,
    app_secret: String,
    endpoints: Endpoints,
    client: reqwest::Client,
    extra_ca: Option<std::path::PathBuf>,
    /// How the WebSocket reaches the platform.
    proxy: pmt_gateway::messaging::proxy::Proxy,
    api: Mutex<Option<Api>>,
    socket: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Slack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slack")
            .field("profile", &self.scope.name().as_str())
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl Slack {
    /// The adapter for `setup`, talking to `endpoints`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup, endpoints: Endpoints) -> Result<Self, Error> {
        let options: SlackOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        let network = &setup.scope.config().network;
        let extra_ca = network.extra_ca_file.as_deref().map(Into::into);
        // Direct unless a proxy is configured or the environment trusted.
        let (builder, proxy) = pmt_gateway::messaging::proxy::client_builder(setup)?;
        let client = builder.build().map_err(|e| {
            Error::new(
                &pmt_core::error::codes::NET_CLIENT_INIT,
                format!("cannot build the Slack client: {e}"),
            )
        })?;
        Ok(Self {
            scope: setup.scope.clone(),
            bot_secret: setup
                .config
                .token_secret
                .as_ref()
                .map_or_else(|| TOKEN_SECRET.to_owned(), |s| s.0.clone()),
            app_secret: options
                .app_token_secret
                .map_or_else(|| APP_TOKEN_SECRET.to_owned(), |s| s.0),
            endpoints,
            client,
            extra_ca,
            proxy,
            api: Mutex::default(),
            socket: Mutex::default(),
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
        let token = credential(&self.scope, &self.bot_secret)
            .map_err(|e| SendError::Permanent(e.message))?;
        Ok(self.api_with(token))
    }

    async fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        let bot = self.api_with(credential(&self.scope, &self.bot_secret)?);
        let app = self.api_with(credential(&self.scope, &self.app_secret)?);
        let me = bot.call("auth.test", &json!({})).await.map_err(|e| {
            if api::unauthorized(&e) {
                socket::lost("unauthorized", "Slack rejected the bot token", false)
            } else {
                socket::lost(
                    "slack_unreachable",
                    format!("Slack: {}", e.describe()),
                    true,
                )
            }
        })?;
        let stop = CancellationToken::new();
        if let Some(old) = self
            .socket
            .lock()
            .expect("socket poisoned")
            .replace(stop.clone())
        {
            old.cancel();
        }
        *self.api.lock().expect("api poisoned") = Some(bot);
        socket::open(socket::Run {
            app,
            link,
            bot_user: me["user_id"].as_str().unwrap_or_default().to_owned(),
            team: me["team_id"].as_str().map(str::to_owned),
            extra_ca: self.extra_ca.clone(),
            proxy: self.proxy.for_websocket(true),
            stop,
        })
        .await
    }
}

/// Block Kit for a message with buttons: the text in sections of at most
/// 3000 characters, then the buttons (5 per actions block).
fn blocks(message: &Outgoing) -> Value {
    let chars: Vec<char> = message.text.chars().collect();
    let mut blocks: Vec<Value> = chars
        .chunks(SECTION_CHARS)
        .map(|chunk| {
            let text: String = chunk.iter().collect();
            json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})
        })
        .collect();
    for (row, buttons) in message.buttons.chunks(5).enumerate() {
        let elements: Vec<Value> = buttons
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let label: String = b.label.chars().take(75).collect();
                json!({
                    "type": "button",
                    "text": {"type": "plain_text", "text": label},
                    "value": b.data,
                    "action_id": format!("pmt_{row}_{i}"),
                })
            })
            .collect();
        blocks.push(json!({"type": "actions", "elements": elements}));
    }
    Value::Array(blocks)
}

/// The Slack name of a reaction emoji (Slack reacts by name).
fn emoji_name(emoji: &str) -> Option<&'static str> {
    const NAMES: &[(&str, &str)] = &[
        ("\u{1f440}", "eyes"),
        ("\u{2705}", "white_check_mark"),
        ("\u{274c}", "x"),
        ("\u{1f44d}", "+1"),
        ("\u{1f44e}", "-1"),
        ("\u{23f3}", "hourglass_flowing_sand"),
        ("\u{1f914}", "thinking_face"),
        ("\u{26a0}\u{fe0f}", "warning"),
        ("\u{1f389}", "tada"),
    ];
    NAMES.iter().find(|(e, _)| *e == emoji).map(|(_, n)| *n)
}

impl Adapter for Slack {
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
            .map_or(39_000, |p| p.max_message_length as usize);
        Descriptor {
            supports_edit: true,
            supports_delete: true,
            supports_reactions: true,
            supports_threads: true,
            interactive: Interactive::Native,
            media_in: all.clone(),
            media_out: all,
            markup: Markup::Platform,
            stateless_send: true,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(self.connect_with(link))
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.socket.lock().expect("socket poisoned").take() {
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
                let mut body = json!({"channel": message.chat_id, "text": message.text});
                if let Some(thread) = &message.thread_id {
                    body["thread_ts"] = json!(thread);
                }
                if !message.buttons.is_empty() {
                    body["blocks"] = blocks(&message);
                }
                if message.plain {
                    body["mrkdwn"] = json!(false);
                }
                let sent = api.call("chat.postMessage", &body).await?;
                first = sent["ts"].as_str().map(str::to_owned);
            }
            for file in &message.attachments {
                api.upload(
                    &message.chat_id,
                    message.thread_id.as_deref(),
                    Path::new(&file.path),
                )
                .await?;
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
            // Empty blocks remove a resolved card's buttons.
            let blocks = if message.buttons.is_empty() {
                json!([])
            } else {
                blocks(message)
            };
            let body = json!({
                "channel": message.chat_id,
                "ts": message_id,
                "text": message.text,
                "blocks": blocks,
            });
            self.api()?.call("chat.update", &body).await.map(drop)
        })
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let body = json!({"channel": chat_id, "ts": message_id});
            self.api()?.call("chat.delete", &body).await.map(drop)
        })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let name = emoji_name(&reaction.emoji).map_or_else(
                || reaction.emoji.trim_matches(':').to_owned(),
                str::to_owned,
            );
            let method = if reaction.remove {
                "reactions.remove"
            } else {
                "reactions.add"
            };
            let body = json!({
                "channel": reaction.chat_id,
                "timestamp": reaction.message_id,
                "name": name,
            });
            self.api()?.call(method, &body).await.map(drop)
        })
    }

    fn resolve_channel<'a>(&'a self, name: &'a str) -> AdapterFuture<'a, Option<String>> {
        Box::pin(async move {
            let api = self.api().ok()?;
            let wanted = name.trim_start_matches('#');
            let mut cursor = String::new();
            // At most ten pages of 1000 conversations.
            for _ in 0..10 {
                let page = api
                    .call(
                        "conversations.list",
                        &json!({"types": "public_channel,private_channel", "limit": 1000,
                                "exclude_archived": true, "cursor": cursor}),
                    )
                    .await
                    .ok()?;
                let found = page["channels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|c| c["name"].as_str() == Some(wanted));
                if let Some(channel) = found {
                    return channel["id"].as_str().map(str::to_owned);
                }
                page["response_metadata"]["next_cursor"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut cursor);
                if cursor.is_empty() {
                    return None;
                }
            }
            None
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let found = self
                .api()?
                .call("conversations.info", &json!({"channel": chat_id}))
                .await?;
            let channel = &found["channel"];
            let dm = channel["is_im"].as_bool() == Some(true);
            let name = channel["name"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| channel["user"].as_str().map(str::to_owned))
                .unwrap_or_else(|| chat_id.to_owned());
            Ok(ChatInfo {
                name,
                chat_type: if dm { ChatType::Dm } else { ChatType::Group },
            })
        })
    }

    fn credential_identity(&self) -> Option<String> {
        credential(&self.scope, &self.bot_secret).ok()
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move { self.api()?.download(&attachment.location).await })
    }
}

#[cfg(test)]
mod tests {
    use pmt_gateway::messaging::Button;

    use super::*;

    #[test]
    fn a_card_is_sections_then_buttons_carrying_their_data() {
        let mut card = Outgoing::text("D1", None, &"x".repeat(3500));
        card.buttons = (0..6)
            .map(|i| Button {
                label: format!("Choice {i}"),
                data: format!("appr:id:{i}:mac"),
            })
            .collect();
        let blocks = blocks(&card);
        let kinds: Vec<&str> = blocks
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["section", "section", "actions", "actions"]);
        assert_eq!(blocks[2]["elements"][0]["value"], "appr:id:0:mac");
        assert_eq!(blocks[3]["elements"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reactions_are_sent_by_name() {
        assert_eq!(emoji_name("\u{1f440}"), Some("eyes"));
        assert_eq!(emoji_name("\u{2705}"), Some("white_check_mark"));
        assert_eq!(emoji_name("nope"), None);
    }
}
