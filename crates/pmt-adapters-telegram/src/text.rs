//! Text messages: `sendMessage` / `editMessageText` in the markup the host
//! rendered (`MarkdownV2`, or plain after a `formatting` refusal), and with
//! rich messages on, the raw Markdown through [`rich`](super::rich) first,
//! then `MarkdownV2` rendered here from the same Markdown. A text longer
//! than one message (a rich-sized reply that fell back, or its plain
//! resend) is cut at 4096 before it is rendered, so no part breaks an
//! entity.

use std::sync::atomic::Ordering;

use pmt_gateway::messaging::markup::render_in;
use pmt_gateway::messaging::split::split;
use pmt_gateway::messaging::{LengthUnit, Markup, Outgoing, SendError};
use serde_json::{Value, json};

use super::api::Api;
use super::rich::{self, Refusal};
use super::{PLATFORM, Telegram, chat, normalize};

/// The longest ordinary message, in UTF-16 code units.
pub(crate) const LIMIT: usize = 4096;

impl Telegram {
    /// Sends `message`'s text; the first message's id.
    pub(crate) async fn send_text(
        &self,
        api: &Api,
        message: &Outgoing,
    ) -> Result<Option<String>, SendError> {
        if !self.raw_markdown(message) {
            let parts = if message.plain {
                split(&message.text, LIMIT, LengthUnit::Utf16)
            } else {
                vec![message.text.clone()]
            };
            return send_parts(api, message, parts, self.markup()).await;
        }
        if rich::fits(&message.text) {
            let mut body = text_body(message, None);
            body["rich_message"] = rich::payload(&message.text);
            match api.call("sendRichMessage", &body).await {
                Ok(sent) => return Ok(normalize::id(&sent["message_id"])),
                Err(err) => self.fall_back(err)?,
            }
        }
        let parts = split(&message.text, LIMIT, LengthUnit::Utf16)
            .iter()
            .map(|part| render_in(Markup::Platform, PLATFORM, part))
            .collect();
        send_parts(api, message, parts, Markup::Platform).await
    }

    /// Edits message `id` to `message`'s text.
    pub(crate) async fn edit_text(
        &self,
        api: &Api,
        id: i64,
        message: &Outgoing,
    ) -> Result<(), SendError> {
        let mut body = json!({
            "chat_id": chat(&message.chat_id),
            "message_id": id,
            "reply_markup": keyboard(message),
        });
        let mut markup = self.markup();
        let mut text = message.text.clone();
        if self.raw_markdown(message) {
            if rich::fits(&text) {
                let mut rich_body = body.clone();
                rich_body["rich_message"] = rich::payload(&text);
                match api.call("editMessageText", &rich_body).await {
                    Ok(_) => return Ok(()),
                    Err(err) => self.fall_back(err)?,
                }
            }
            text = render_in(Markup::Platform, PLATFORM, &text);
            markup = Markup::Platform;
        }
        body["text"] = json!(text);
        if let Some(mode) = parse_mode(message, markup) {
            body["parse_mode"] = json!(mode);
        }
        api.call("editMessageText", &body).await.map(drop)
    }

    /// `message` holds the agent's Markdown for a rich message.
    fn raw_markdown(&self, message: &Outgoing) -> bool {
        !message.plain && self.markup() == Markup::Markdown
    }

    /// `Ok` when a refused rich call falls back to `MarkdownV2`; the error
    /// itself when it must not be resent (the request may have arrived).
    fn fall_back(&self, err: SendError) -> Result<(), SendError> {
        match rich::refusal(&err) {
            Some(Refusal::Message) => {
                tracing::debug!(
                    "rich message refused ({}); sending MarkdownV2",
                    err.describe()
                );
                Ok(())
            }
            Some(Refusal::Unavailable) => {
                if !self.rich_unavailable.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "the Bot API has no rich messages ({}); sending MarkdownV2 from now on",
                        err.describe()
                    );
                }
                Ok(())
            }
            None => Err(err),
        }
    }
}

/// Sends `parts` in order (the first answers the message `message`
/// replies to; the last carries its buttons); the first one's id.
async fn send_parts(
    api: &Api,
    message: &Outgoing,
    parts: Vec<String>,
    markup: Markup,
) -> Result<Option<String>, SendError> {
    let last = parts.len().saturating_sub(1);
    let mut first = None;
    for (i, text) in parts.into_iter().enumerate() {
        let part = Outgoing {
            text,
            reply_to: message.reply_to.clone().filter(|_| i == 0),
            buttons: if i == last {
                message.buttons.clone()
            } else {
                Vec::new()
            },
            ..message.clone()
        };
        let sent = api
            .call("sendMessage", &text_body(&part, Some(markup)))
            .await?;
        first = first.or_else(|| normalize::id(&sent["message_id"]));
    }
    Ok(first)
}

pub(crate) fn keyboard(message: &Outgoing) -> Value {
    let rows: Vec<Value> = message
        .buttons
        .iter()
        .map(|b| json!([{"text": b.label, "callback_data": b.data}]))
        .collect();
    json!({"inline_keyboard": rows})
}

/// The Bot API `parse_mode` of text in `markup` (none for a plain resend).
fn parse_mode(message: &Outgoing, markup: Markup) -> Option<&'static str> {
    if message.plain {
        return None;
    }
    match markup {
        Markup::Html => Some("HTML"),
        Markup::Platform => Some("MarkdownV2"),
        Markup::Plain | Markup::Markdown => None,
    }
}

/// The body of a message: its text in `markup` (none for a rich message,
/// which carries `rich_message` instead), thread, the message it answers
/// and its buttons.
fn text_body(message: &Outgoing, markup: Option<Markup>) -> Value {
    let mut body = json!({"chat_id": chat(&message.chat_id)});
    if let Some(markup) = markup {
        body["text"] = json!(message.text);
        if let Some(mode) = parse_mode(message, markup) {
            body["parse_mode"] = json!(mode);
        }
    }
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
    if message.notify == Some(false) {
        body["disable_notification"] = json!(true);
    }
    body
}
