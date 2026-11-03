//! The first-party Signal adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; First-party adapter transports and
//! required settings): an external signal-cli HTTP daemon at
//! `options.http_url` for the account `options.account` (E.164), events
//! from its server-sent stream and sends, edits, reactions, typing and
//! attachments over JSON-RPC. Signal has no buttons, so cards are
//! numbered. The account number identifies the credential lock.

mod events;
/// The mock platform the conformance kit runs this adapter against; only
/// in builds with the `kit` feature (tests, `prometheus` with
/// `adapter-kits`), so release binaries do not carry it.
#[cfg(feature = "kit")]
pub mod mock;
mod rpc;

use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use pmt_config::platforms::SignalOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, Attachment,
    ChatInfo, ChatType, Descriptor, Markup, MediaKind, MediaStream, Outgoing, Private, Reaction,
    SendError, Sent, markup,
};
use rpc::Rpc;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "signal";

/// The factory the daemon registers for `signal`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Signal::new(setup)?) as Arc<dyn Adapter>))
}

/// A signal-cli account of one profile.
pub struct Signal {
    rpc: Option<Rpc>,
    missing: Option<&'static str>,
    stream: Mutex<Option<CancellationToken>>,
    /// The daemon refused text styles once: send plain text from then on.
    no_styles: std::sync::atomic::AtomicBool,
    reader: events::Reader,
    /// When the typing indicator was last shown, by chat.
    typing: Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// How often the typing indicator is refreshed (it lasts about 15 s).
const TYPING_EVERY: std::time::Duration = std::time::Duration::from_secs(8);

impl std::fmt::Debug for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signal")
            .field("rpc", &self.rpc)
            .finish_non_exhaustive()
    }
}

/// Where a message goes: a group by id, or a person.
fn recipient(chat: &str) -> Value {
    match chat.strip_prefix(events::GROUP) {
        Some(group) => json!({"groupId": group}),
        None => json!({"recipient": [chat]}),
    }
}

/// Whether a refusal says the daemon does not know text styles.
fn styles_refused(err: &SendError) -> bool {
    matches!(err, SendError::Permanent(why) if why.to_lowercase().contains("textstyle")
        || why.to_lowercase().contains("text-style"))
}

/// `<author>:<timestamp>` split.
fn message_ref(id: &str) -> Result<(&str, i64), SendError> {
    id.rsplit_once(':')
        .and_then(|(author, ts)| Some((author, ts.parse().ok()?)))
        .ok_or_else(|| SendError::Permanent(format!("'{id}' is not a Signal message id")))
}

impl Signal {
    /// Sets the text of `params`: the host leaves Markdown, sent as plain
    /// text with native style ranges (none for a plain resend or once the
    /// daemon refused styles).
    fn styled(&self, params: &mut Value, message: &Outgoing) {
        let plain = message.plain || self.no_styles.load(std::sync::atomic::Ordering::Relaxed);
        if plain {
            params["message"] = json!(message.text);
            return;
        }
        let (text, styles) = markup::signal_styles(&message.text);
        params["message"] = json!(text);
        if !styles.is_empty() {
            let specs: Vec<String> = styles.iter().map(markup::Style::spec).collect();
            params["textStyle"] = json!(specs);
        }
    }

    /// Calls `send`; a daemon without text-style support gets the plain
    /// text instead, with one WARN.
    async fn call_styled(
        &self,
        rpc: &Rpc,
        mut params: Value,
        message: &Outgoing,
    ) -> Result<Value, SendError> {
        match rpc.call("send", &params).await {
            Err(err) if params.get("textStyle").is_some() && styles_refused(&err) => {
                if !self
                    .no_styles
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        platform = PLATFORM,
                        "the signal-cli daemon does not support text styles; sending plain text"
                    );
                }
                params.as_object_mut().map(|p| p.remove("textStyle"));
                params["message"] =
                    json!(markup::render_in(Markup::Plain, PLATFORM, &message.text));
                rpc.call("send", &params).await
            }
            other => other,
        }
    }

    /// The adapter for `setup`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options; the HTTP client cannot be built.
    pub fn new(setup: &AdapterSetup) -> Result<Self, Error> {
        let options: SignalOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        // Direct unless a proxy is configured or the environment trusted.
        let (builder, _) = pmt_gateway::messaging::proxy::client_builder(setup)?;
        let client = builder.build().map_err(|e| {
            Error::new(
                &pmt_core::error::codes::NET_CLIENT_INIT,
                format!("cannot build the Signal client: {e}"),
            )
        })?;
        let options_groups = options.allow_groups.clone();
        let rpc_account = options.account.clone();
        let (rpc, missing) = match (options.http_url, options.account) {
            (Some(url), Some(account)) => (
                Some(Rpc {
                    client,
                    url: url.trim_end_matches('/').to_owned(),
                    account,
                    ids: Arc::new(AtomicU64::new(1)),
                }),
                None,
            ),
            (None, _) => (None, Some("http_url")),
            (_, None) => (None, Some("account")),
        };
        Ok(Self {
            rpc,
            missing,
            stream: Mutex::default(),
            no_styles: std::sync::atomic::AtomicBool::new(false),
            reader: events::Reader {
                account: rpc_account.unwrap_or_default(),
                allow_groups: options_groups,
                sent: Arc::default(),
                to_self: Arc::default(),
            },
            typing: Mutex::default(),
        })
    }

    fn rpc(&self) -> Result<&Rpc, SendError> {
        self.rpc
            .as_ref()
            .ok_or_else(|| SendError::Permanent(self.missing_option().message))
    }

    fn missing_option(&self) -> AdapterError {
        AdapterError {
            retryable: false,
            code: "missing_option".to_owned(),
            message: format!(
                "gateway.platforms.signal.options.{} is required",
                self.missing.unwrap_or("http_url")
            ),
        }
    }

    async fn connect_with(&self, link: Arc<dyn AdapterLink>) -> Result<(), AdapterError> {
        let rpc = self.rpc.as_ref().ok_or_else(|| self.missing_option())?;
        let unreachable = |why: String| AdapterError {
            retryable: true,
            code: "signal_daemon_unreachable".to_owned(),
            message: format!("the signal-cli daemon at {} is unreachable: {why}", rpc.url),
        };
        rpc.check().await.map_err(unreachable)?;
        let response = rpc.events().await.map_err(unreachable)?;
        tracing::info!(
            platform = PLATFORM,
            account = events::redact(&rpc.account),
            "connected to signal-cli"
        );
        let stop = CancellationToken::new();
        if let Some(old) = self
            .stream
            .lock()
            .expect("stream poisoned")
            .replace(stop.clone())
        {
            old.cancel();
        }
        tokio::spawn(events::follow(
            response,
            link,
            self.reader.clone(),
            rpc.clone(),
            stop,
        ));
        Ok(())
    }
}

impl Adapter for Signal {
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
            .map_or(8000, |p| p.max_message_length as usize);
        Descriptor {
            markup: Markup::Platform,
            supports_edit: true,
            supports_delete: true,
            supports_typing: true,
            supports_reactions: true,
            media_in: all.clone(),
            media_out: all,
            stateless_send: true,
            private_replies: true,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn reply_privately<'a>(&'a self, to: &'a Private) -> AdapterFuture<'a, Result<(), SendError>> {
        // A direct message: a person's own chat is their number.
        Box::pin(async move {
            self.send(&Outgoing::text(&to.sender_id, None, &to.text))
                .await
                .map(drop)
        })
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(self.connect_with(link))
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.stream.lock().expect("stream poisoned").take() {
            stop.cancel();
        }
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            let rpc = self.rpc()?;
            let mut params = recipient(&message.chat_id);
            self.styled(&mut params, &message);
            if !message.attachments.is_empty() {
                let paths: Vec<&str> = message
                    .attachments
                    .iter()
                    .map(|a| a.path.as_str())
                    .collect();
                params["attachments"] = json!(paths);
            }
            if let Some(reply) = &message.reply_to {
                let (author, ts) = message_ref(reply)?;
                params["quoteAuthor"] = json!(author);
                params["quoteTimestamp"] = json!(ts);
            }
            if message.chat_id == rpc.account {
                self.reader
                    .sending_to_self(params["message"].as_str().unwrap_or_default());
            }
            let sent = self.call_styled(rpc, params, &message).await?;
            if let Some(timestamp) = sent["timestamp"].as_i64() {
                self.reader.sent(timestamp);
            }
            let id = sent["timestamp"]
                .as_i64()
                .map(|ts| format!("{}:{ts}", rpc.account));
            Ok(Sent { message_id: id })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let (_, ts) = message_ref(message_id)?;
            let mut params = recipient(&message.chat_id);
            self.styled(&mut params, message);
            params["editTimestamp"] = json!(ts);
            let rpc = self.rpc()?;
            self.call_styled(rpc, params, message).await.map(drop)
        })
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let (_, ts) = message_ref(message_id)?;
            let mut params = recipient(chat_id);
            params["targetTimestamp"] = json!(ts);
            self.rpc()?.call("remoteDelete", &params).await.map(drop)
        })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let (author, ts) = message_ref(&reaction.message_id)?;
            let mut params = recipient(&reaction.chat_id);
            params["emoji"] = json!(reaction.emoji);
            params["targetAuthor"] = json!(author);
            params["targetTimestamp"] = json!(ts);
            params["remove"] = json!(reaction.remove);
            self.rpc()?.call("sendReaction", &params).await.map(drop)
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            let Some(group) = chat_id.strip_prefix(events::GROUP) else {
                return Ok(ChatInfo {
                    name: events::redact(chat_id),
                    chat_type: ChatType::Dm,
                });
            };
            let groups = self.rpc()?.call("listGroups", &json!({})).await?;
            let name = groups
                .as_array()
                .into_iter()
                .flatten()
                .find(|g| g["id"].as_str() == Some(group))
                .and_then(|g| g["name"].as_str())
                .unwrap_or(group)
                .to_owned();
            Ok(ChatInfo {
                name,
                chat_type: ChatType::Group,
            })
        })
    }

    fn send_typing<'a>(
        &'a self,
        chat_id: &'a str,
        _thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let now = std::time::Instant::now();
            {
                let mut shown = self.typing.lock().expect("typing poisoned");
                if shown
                    .get(chat_id)
                    .is_some_and(|at| now.duration_since(*at) < TYPING_EVERY)
                {
                    return Ok(());
                }
                shown.insert(chat_id.to_owned(), now);
            }
            self.rpc()?
                .call("sendTyping", &recipient(chat_id))
                .await
                .map(drop)
        })
    }

    fn credential_identity(&self) -> Option<String> {
        self.rpc.as_ref().map(|r| r.account.clone())
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move {
            let found = self
                .rpc()?
                .call("getAttachment", &json!({"id": attachment.location}))
                .await?;
            let encoded = found["data"]
                .as_str()
                .or_else(|| found.as_str())
                .unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| SendError::Permanent(format!("the attachment is not base64: {e}")))?;
            let declared_len = Some(bytes.len() as u64);
            Ok(MediaStream {
                declared_len,
                chunks: Box::pin(futures_util::stream::iter([Ok(bytes)])),
            })
        })
    }
}
