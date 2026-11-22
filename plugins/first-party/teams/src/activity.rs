//! Inbound activities: messages (and Adaptive Card actions) to inbound
//! messages. Personal chats are always heard; group chats only when the
//! bot is @mentioned or a message replies to one of the bot's; channel
//! messages the same unless `require_mention = false`, which admits every
//! message resource-specific consent delivers. The `<at>Bot</at>` mention
//! is stripped. Users are their Entra ID (AAD) object ids. Images and
//! file attachments are downloaded (the bot token only goes to Bot
//! Connector hosts).

use adapter_kit::message::{InFile, Inbound, kind_of_name};
use adapter_kit::{host, net};
use serde_json::Value;

use crate::Teams;

/// The prefix Teams gives a bot's channel account id.
const BOT_PREFIX: &str = "28:";

fn chat_type(activity: &Value) -> &'static str {
    match activity["conversation"]["conversationType"].as_str() {
        Some("groupChat") => "group",
        Some("channel") => "channel",
        _ => "dm",
    }
}

/// `text` without `<at>...</at>` mention tags.
pub(crate) fn strip_mentions(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("<at>") {
        out.push_str(&rest[..open]);
        match rest[open..].find("</at>") {
            Some(close) => rest = &rest[open + close + "</at>".len()..],
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Teams {
    fn is_bot(&self, id: &str) -> bool {
        let bare = id.strip_prefix(BOT_PREFIX).unwrap_or(id);
        bare == self.client_id
    }

    fn mentions_bot(&self, activity: &Value) -> bool {
        let text = activity["text"].as_str().unwrap_or_default();
        let mentions: Vec<&Value> = activity["entities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "mention")
            .collect();
        if mentions.is_empty() {
            return text.contains("<at>");
        }
        let recipient = activity["recipient"]["id"].as_str().unwrap_or_default();
        mentions.iter().any(|m| {
            let id = m["mentioned"]["id"].as_str().unwrap_or_default();
            id == recipient || self.is_bot(id)
        })
    }

    /// Takes one verified activity in.
    pub(crate) fn activity(&mut self, activity: &Value) {
        let Some(chat) = activity["conversation"]["id"].as_str() else {
            return;
        };
        if let Some(url) = activity["serviceUrl"].as_str() {
            self.remember_service(chat, url);
        }
        let from = &activity["from"];
        if self.is_bot(from["id"].as_str().unwrap_or_default()) {
            return;
        }
        let callback = card_action(activity);
        let kind = chat_type(activity);
        let replies_to_bot = activity["replyToId"]
            .as_str()
            .is_some_and(|id| self.sent.iter().any(|s| s == id));
        let mentioned = self.mentions_bot(activity);
        let required = kind == "group" || (kind == "channel" && self.require_mention);
        if callback.is_none() && required && !mentioned && !replies_to_bot {
            return;
        }
        let sender = from["aadObjectId"]
            .as_str()
            .or_else(|| from["id"].as_str())
            .unwrap_or_default();
        let id = activity["id"].as_str().unwrap_or_default();
        let text = strip_mentions(activity["text"].as_str().unwrap_or_default());
        let mut inbound = Inbound::text(id, chat, kind, sender, &text);
        inbound.sender_name = from["name"].as_str().map(str::to_owned);
        inbound.mentioned = mentioned;
        inbound.replies_to_bot = replies_to_bot;
        inbound.reply_to = activity["replyToId"].as_str().map(str::to_owned);
        if callback.is_some() {
            inbound.callback = callback;
        } else if let Some(data) = self.numbered_answer(chat, &text) {
            inbound.callback = Some(data);
        }
        if inbound.callback.is_some() {
            // A pressed card answers it: its numbers mean nothing now.
            self.choices.retain(|c| c.chat != chat);
        }
        for attachment in activity["attachments"].as_array().into_iter().flatten() {
            if let Some(file) = self.download(attachment) {
                inbound.attachments.push(file);
            }
        }
        self.inbox.push(inbound);
    }

    fn download(&mut self, attachment: &Value) -> Option<InFile> {
        let content_type = attachment["contentType"].as_str().unwrap_or_default();
        let name = attachment["name"]
            .as_str()
            .unwrap_or("attachment")
            .to_owned();
        let (url, authorized, kind) =
            if content_type == "application/vnd.microsoft.teams.file.download.info" {
                // A pre-authorized SharePoint link: no bot token.
                (
                    attachment["content"]["downloadUrl"].as_str()?,
                    false,
                    kind_of_name(&name),
                )
            } else if content_type.starts_with("image/") {
                let url = attachment["contentUrl"].as_str()?;
                (url, self.is_service_url(url), "image")
            } else {
                return None;
            };
        let bearer = if authorized {
            self.auth.bearer().ok()
        } else {
            None
        };
        let headers: Vec<(&str, &str)> = bearer
            .as_deref()
            .map(|b| vec![("authorization", b)])
            .unwrap_or_default();
        match net::send("GET", url, &headers, None) {
            Ok(response) if response.status < 300 => {
                Some(InFile::bytes(kind, &name, response.body))
            }
            other => {
                host::warn(&format!(
                    "could not download a Teams attachment: {:?}",
                    other.map(|r| r.status)
                ));
                None
            }
        }
    }
}

/// The button data of an Adaptive Card action: an `Action.Submit` posts a
/// message whose `value` holds it, an `Action.Execute` an
/// `adaptiveCard/action` invoke.
pub(crate) fn card_action(activity: &Value) -> Option<String> {
    let value = &activity["value"];
    let data = if activity["type"] == "invoke" {
        &value["action"]["data"]
    } else {
        value
    };
    data["prometheus"].as_str().map(str::to_owned)
}
