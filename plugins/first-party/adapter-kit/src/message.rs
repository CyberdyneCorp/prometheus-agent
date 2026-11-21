//! The JSON forms the host exchanges with an adapter: polled inbound
//! messages and the outgoing message `send` receives. Files cross as
//! base64: an outgoing message carries its attachments' bytes, and an
//! inbound one carries either the bytes the plugin downloaded (the host
//! keeps them for the gateway's size cap and cache) or a platform
//! reference the gateway passes on as it is.

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
    /// Files it carries.
    pub attachments: Vec<InFile>,
}

/// An inbound file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InFile {
    /// `image`, `audio`, `voice`, `video`, `document`, `location`.
    pub kind: &'static str,
    /// The file name.
    pub name: Option<String>,
    /// The bytes, when the plugin downloaded them.
    pub data: Option<Vec<u8>>,
    /// A platform reference (or a `geo:` URI), when it did not.
    pub location: Option<String>,
    /// A local file a platform daemon on this machine saved; the host
    /// reads it only inside the plugin's `fs_read` grant.
    pub path: Option<String>,
    /// Length of a voice or audio clip.
    pub duration_secs: Option<u64>,
}

impl InFile {
    /// A downloaded file.
    #[must_use]
    pub fn bytes(kind: &'static str, name: &str, data: Vec<u8>) -> Self {
        Self {
            kind,
            name: Some(name.to_owned()),
            data: Some(data),
            ..Self::default()
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("kind".into(), json!(self.kind));
        if let Some(name) = &self.name {
            map.insert("name".into(), json!(name));
        }
        if let Some(data) = &self.data {
            map.insert("data".into(), json!(crate::sign::base64(data)));
        }
        if let Some(location) = &self.location {
            map.insert("location".into(), json!(location));
        }
        if let Some(path) = &self.path {
            map.insert("path".into(), json!(path));
        }
        if let Some(secs) = self.duration_secs {
            map.insert("duration_secs".into(), json!(secs));
        }
        Value::Object(map)
    }
}

/// The media kind of a file name, by extension (as the host names kinds).
#[must_use]
pub fn kind_of_name(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => "image",
        Some("ogg" | "oga" | "opus" | "mp3" | "m4a" | "wav" | "flac" | "amr" | "silk") => "audio",
        Some("mp4" | "mov" | "webm") => "video",
        _ => "document",
    }
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
        if !self.attachments.is_empty() {
            let files = self.attachments.iter().map(InFile::to_value).collect();
            map.insert("attachments".into(), Value::Array(files));
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

/// A file of an outgoing message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutFile {
    /// The file name.
    pub name: String,
    /// `image`, `audio`, `voice`, `video` or `document`.
    pub kind: String,
    /// Its MIME type.
    pub mime: String,
    /// Sent as a document (images uncompressed).
    pub as_document: bool,
    /// Sent as a voice note.
    pub as_voice: bool,
    /// The bytes.
    pub data: Vec<u8>,
    /// Where the file lies on this machine, for a platform daemon that
    /// sends by path (SimpleX).
    pub path: Option<String>,
}

/// The message `send` receives. Besides a send (`op` empty), the host
/// asks through `send` for what the descriptor declares: `edit` (of
/// `message_id`, with the new text), `delete`, `react` (`emoji`, `remove`)
/// and `typing`; a plugin answers any other `op` with `unsupported`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outgoing {
    /// `""` for a send, else `edit`, `delete`, `react` or `typing`.
    pub op: String,
    /// The message an edit, delete or reaction is about.
    pub message_id: Option<String>,
    /// A reaction's emoji.
    pub emoji: Option<String>,
    /// Remove the reaction instead of adding it.
    pub remove: bool,
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
    /// Files (adapters whose descriptor declares `media_out`).
    pub attachments: Vec<OutFile>,
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
        let mut attachments = Vec::new();
        for file in value["attachments"].as_array().into_iter().flatten() {
            let field = |key: &str| file[key].as_str().unwrap_or_default().to_owned();
            attachments.push(OutFile {
                name: field("name"),
                kind: field("kind"),
                mime: field("mime"),
                as_document: file["as_document"].as_bool().unwrap_or(false),
                as_voice: file["as_voice"].as_bool().unwrap_or(false),
                path: file["path"].as_str().map(str::to_owned),
                data: crate::sign::base64_decode(file["data"].as_str().unwrap_or_default())
                    .map_err(|e| format!("permanent: attachment {}: {e}", field("name")))?,
            });
        }
        Ok(Self {
            op: text("op").unwrap_or_default(),
            message_id: text("message_id"),
            emoji: text("emoji"),
            remove: value["remove"].as_bool().unwrap_or(false),
            chat: text("chat").unwrap_or_default(),
            thread: text("thread"),
            reply_to: text("reply_to"),
            text: text("text").unwrap_or_default(),
            buttons,
            attachments,
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
        assert!(out.attachments.is_empty());
    }

    #[test]
    fn files_cross_as_base64_both_ways() {
        let out = Outgoing::parse(
            r#"{"chat":"c","text":"","attachments":[{"name":"a.png","kind":"image","mime":"image/png","data":"aGk="}]}"#,
        )
        .unwrap();
        assert_eq!(out.attachments[0].data, b"hi");
        let mut message = Inbound::text("1", "c", "dm", "u", "");
        message
            .attachments
            .push(InFile::bytes("image", "b.png", b"hi".to_vec()));
        let value = message.to_value();
        assert_eq!(value["attachments"][0]["data"], "aGk=");
        assert_eq!(kind_of_name("v.OGG"), "audio");
    }
}
