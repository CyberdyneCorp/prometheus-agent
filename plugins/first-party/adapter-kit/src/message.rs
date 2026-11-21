//! The JSON forms the host exchanges with an adapter: polled inbound
//! messages and the outgoing message `send` receives.

use serde_json::{Map, Value, json};

/// One inbound message for `poll`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inbound {
    /// Platform message id (the host deduplicates by chat and id).
    pub id: String,
    /// Chat id.
    pub chat: String,
    /// `dm`, `group`, `channel` or `thread`.
    pub chat_type: &'static str,
    /// Canonical sender id.
    pub sender: String,
    /// Sender display name.
    pub sender_name: Option<String>,
    /// The text.
    pub text: String,
    /// Thread id.
    pub thread: Option<String>,
    /// The message it answers.
    pub reply_to: Option<String>,
    /// A pressed button's data.
    pub callback: Option<String>,
    /// It mentions the bot.
    pub mentioned: bool,
    /// It replies to the bot.
    pub replies_to_bot: bool,
    /// Sent by a bot.
    pub is_bot: bool,
}

impl Inbound {
    /// A text message.
    #[must_use]
    pub fn text(id: &str, chat: &str, chat_type: &'static str, sender: &str, text: &str) -> Self {
        Self {
            id: id.to_owned(),
            chat: chat.to_owned(),
            chat_type,
            sender: sender.to_owned(),
            text: text.to_owned(),
            ..Self::default()
        }
    }

    /// Its JSON form.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), json!(self.id));
        map.insert("chat".into(), json!(self.chat));
        map.insert("chat_type".into(), json!(self.chat_type));
        map.insert("sender".into(), json!(self.sender));
        map.insert("text".into(), json!(self.text));
        let optional = [
            ("sender_name", &self.sender_name),
            ("thread", &self.thread),
            ("reply_to", &self.reply_to),
            ("callback", &self.callback),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                map.insert(key.into(), json!(value));
            }
        }
        for (key, flag) in [
            ("mentioned", self.mentioned),
            ("replies_to_bot", self.replies_to_bot),
            ("is_bot", self.is_bot),
        ] {
            if flag {
                map.insert(key.into(), json!(true));
            }
        }
        Value::Object(map)
    }
}

/// The JSON `poll` answers for `messages`.
#[must_use]
pub fn batch(messages: &[Inbound]) -> String {
    Value::Array(messages.iter().map(Inbound::to_value).collect()).to_string()
}

/// A button of an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// What it shows.
    pub label: String,
    /// What a press sends back (the inbound `callback`).
    pub data: String,
}

/// The message `send` receives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outgoing {
    /// Chat id.
    pub chat: String,
    /// Thread id.
    pub thread: Option<String>,
    /// The message it answers.
    pub reply_to: Option<String>,
    /// The text (already split to the descriptor's limit).
    pub text: String,
    /// Buttons, for a `native` interactive tier.
    pub buttons: Vec<Button>,
}

impl Outgoing {
    /// Parses the host's JSON.
    ///
    /// # Errors
    ///
    /// `permanent: ...` for malformed JSON.
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(json).map_err(|e| format!("permanent: {e}"))?;
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        let buttons = value["buttons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                Some(Button {
                    label: b["label"].as_str()?.to_owned(),
                    data: b["data"].as_str()?.to_owned(),
                })
            })
            .collect();
        Ok(Self {
            chat: text("chat").unwrap_or_default(),
            thread: text("thread"),
            reply_to: text("reply_to"),
            text: text("text").unwrap_or_default(),
            buttons,
        })
    }
}

/// `{"name", "chat_type"}` for `chat-info`.
#[must_use]
pub fn chat_info(name: &str, chat_type: &str) -> String {
    json!({"name": name, "chat_type": chat_type}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_json_carries_only_set_fields() {
        let mut message = Inbound::text("1", "c", "group", "u", "hi");
        message.mentioned = true;
        let value = message.to_value();
        assert_eq!(value["mentioned"], true);
        assert!(value.get("thread").is_none());
        assert!(value.get("is_bot").is_none());
    }

    #[test]
    fn outgoing_parses_the_host_form() {
        let out = Outgoing::parse(
            r#"{"chat":"c","thread":null,"reply_to":"9","text":"t","buttons":[{"label":"Yes","data":"y"}]}"#,
        )
        .unwrap();
        assert_eq!(out.reply_to.as_deref(), Some("9"));
        assert_eq!(out.buttons[0].data, "y");
    }
}
