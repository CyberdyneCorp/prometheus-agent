//! The capability descriptor (spec: platform-adapters, Capability
//! descriptor is the single source of truth): everything an adapter
//! supports, declared once before `connect`. The gateway reads limits and
//! capabilities only from here; a descriptor missing a required field or
//! with a non-positive limit fails the adapter's load.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How choices are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interactive {
    /// Platform buttons.
    Native,
    /// A numbered list answered by number.
    Numbered,
    /// Slash-command instructions.
    None,
}

/// An adapter's own access policy, when it enforces one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    /// It submits only senders on its allowlist (trusted).
    Allowlist,
    /// It submits everyone (not trusted).
    Open,
}

/// The unit a platform measures message length in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LengthUnit {
    /// Unicode scalar values.
    #[default]
    Chars,
    /// UTF-16 code units (an astral-plane character counts 2).
    Utf16,
    /// UTF-8 bytes.
    Utf8Bytes,
}

impl LengthUnit {
    /// The length of `text` in this unit.
    #[must_use]
    pub fn len(self, text: &str) -> usize {
        match self {
            Self::Chars => text.chars().count(),
            Self::Utf16 => text.encode_utf16().count(),
            Self::Utf8Bytes => text.len(),
        }
    }

    /// The length of one character in this unit.
    #[must_use]
    pub fn of(self, c: char) -> usize {
        match self {
            Self::Chars => 1,
            Self::Utf16 => c.len_utf16(),
            Self::Utf8Bytes => c.len_utf8(),
        }
    }
}

/// The markup a platform renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Markup {
    /// Plain text.
    #[default]
    Plain,
    /// Markdown.
    Markdown,
    /// HTML.
    Html,
    /// The platform's own dialect.
    Platform,
}

/// Media kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A picture.
    Image,
    /// An audio file.
    Audio,
    /// A voice note.
    Voice,
    /// A video.
    Video,
    /// Any other file.
    Document,
}

/// Where a scheduled job's continuation lands (spec: scheduled-jobs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuableSurface {
    /// A thread of the delivered message.
    #[default]
    Thread,
    /// The channel itself.
    InChannel,
}

/// What an adapter declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // independent capability flags
pub struct Descriptor {
    /// Platform name.
    pub platform: String,
    /// Longest message, in [`length_unit`](Self::length_unit).
    pub max_message_length: usize,
    /// The unit of the limit.
    pub length_unit: LengthUnit,
    /// The adapter splits long messages itself (the host hands it whole
    /// replies).
    pub splits_long_messages: bool,
    /// Messages can be edited.
    pub supports_edit: bool,
    /// Messages can be deleted.
    pub supports_delete: bool,
    /// A typing indicator can be shown.
    pub supports_typing: bool,
    /// It can add and remove reactions.
    pub supports_reactions: bool,
    /// Messages can live in threads.
    pub supports_threads: bool,
    /// How choices are rendered.
    pub interactive: Interactive,
    /// Media it receives.
    pub media_in: BTreeSet<MediaKind>,
    /// Media it sends.
    pub media_out: BTreeSet<MediaKind>,
    /// The platform reports delivery.
    pub delivery_receipts: bool,
    /// The platform reports reads.
    pub read_receipts: bool,
    /// It plays audio while a reply streams.
    pub streaming_tts: bool,
    /// The markup it renders.
    pub markup: Markup,
    /// It is served on the daemon's shared HTTP listener.
    pub binds_http: bool,
    /// It can read a chat's earlier messages.
    #[serde(default)]
    pub can_fetch_history: bool,
    /// It reports the sender's platform roles.
    #[serde(default)]
    pub exposes_roles: bool,
    /// Direct messages have topics.
    #[serde(default)]
    pub dm_topics: bool,
    /// The platform streams drafts natively.
    #[serde(default)]
    pub native_stream: bool,
    /// The platform shows task cards.
    #[serde(default)]
    pub task_cards: bool,
    /// The platform shows a status line.
    #[serde(default)]
    pub status_line: bool,
    /// It can send one message without a running connection (one-shot
    /// `gateway send` without the daemon).
    #[serde(default)]
    pub stateless_send: bool,
    /// It marks messages not addressed to the bot.
    #[serde(default)]
    pub not_addressed_to_bot: bool,
    /// Where a job's continuation lands.
    #[serde(default)]
    pub continuable_surface: ContinuableSurface,
    /// The connector authorizes senders upstream (the relay only).
    #[serde(default)]
    pub authorization_is_upstream: bool,
    /// A character that stands for `/` in front of a command name where
    /// the platform blocks slash commands (Slack threads: `!stop`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_prefix_alias: Option<String>,
    /// The adapter's own access policy (`enforces_own_access_policy`).
    #[serde(
        default,
        rename = "enforces_own_access_policy",
        skip_serializing_if = "Option::is_none"
    )]
    pub access_policy: Option<AccessPolicy>,
}

/// Fields every descriptor must state.
const REQUIRED: &[&str] = &[
    "platform",
    "max_message_length",
    "length_unit",
    "splits_long_messages",
    "supports_edit",
    "supports_delete",
    "supports_typing",
    "supports_reactions",
    "supports_threads",
    "interactive",
    "media_in",
    "media_out",
    "delivery_receipts",
    "read_receipts",
    "streaming_tts",
    "markup",
    "binds_http",
];

impl Descriptor {
    /// A plain descriptor for `platform`: text only, counted in
    /// characters, no editing, numbered choices.
    #[must_use]
    pub fn new(platform: &str, max_message_length: usize) -> Self {
        Self {
            platform: platform.to_owned(),
            max_message_length,
            length_unit: LengthUnit::Chars,
            splits_long_messages: false,
            supports_edit: false,
            supports_delete: false,
            supports_typing: false,
            supports_reactions: false,
            supports_threads: false,
            interactive: Interactive::Numbered,
            media_in: BTreeSet::new(),
            media_out: BTreeSet::new(),
            delivery_receipts: false,
            read_receipts: false,
            streaming_tts: false,
            markup: Markup::Plain,
            binds_http: false,
            can_fetch_history: false,
            exposes_roles: false,
            dm_topics: false,
            native_stream: false,
            task_cards: false,
            status_line: false,
            stateless_send: false,
            not_addressed_to_bot: false,
            continuable_surface: ContinuableSurface::Thread,
            authorization_is_upstream: false,
            access_policy: None,
            command_prefix_alias: None,
        }
    }

    /// Reads a descriptor an adapter published as JSON (a plugin's
    /// `describe()`).
    ///
    /// # Errors
    ///
    /// "descriptor field <name> is required" for a missing field, else
    /// what [`check`](Self::check) refuses or the type mismatch.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        if let Some(missing) = REQUIRED.iter().find(|f| value.get(**f).is_none()) {
            return Err(format!("descriptor field {missing} is required"));
        }
        if value["max_message_length"].as_i64().is_some_and(|n| n <= 0) {
            return Err(non_positive());
        }
        let descriptor: Self = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid descriptor: {e}"))?;
        descriptor.check()?;
        Ok(descriptor)
    }

    /// Refuses a descriptor the gateway cannot use.
    ///
    /// # Errors
    ///
    /// A non-positive limit or an empty or upper-case platform name (the
    /// host refuses `authorization_is_upstream` on anything but the
    /// relay).
    pub fn check(&self) -> Result<(), String> {
        if self.max_message_length == 0 {
            return Err(non_positive());
        }
        if self.platform.is_empty() || self.platform != self.platform.to_lowercase() {
            return Err(format!(
                "descriptor field platform must be a lower-case name (found '{}')",
                self.platform
            ));
        }
        Ok(())
    }

    /// Whether `kind` can be sent.
    #[must_use]
    pub fn sends(&self, kind: MediaKind) -> bool {
        self.media_out.contains(&kind)
    }
}

fn non_positive() -> String {
    "descriptor field max_message_length must be a positive integer".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_without_a_limit_fails_to_load() {
        let mut value = serde_json::to_value(Descriptor::new("mattermost", 16_383)).unwrap();
        assert_eq!(
            Descriptor::from_value(&value).unwrap().max_message_length,
            16_383
        );
        value.as_object_mut().unwrap().remove("max_message_length");
        assert_eq!(
            Descriptor::from_value(&value).unwrap_err(),
            "descriptor field max_message_length is required"
        );
        value["max_message_length"] = serde_json::json!(0);
        assert!(
            Descriptor::from_value(&value)
                .unwrap_err()
                .contains("positive")
        );
        value["max_message_length"] = serde_json::json!(-5);
        assert!(
            Descriptor::from_value(&value)
                .unwrap_err()
                .contains("positive")
        );
    }

    #[test]
    fn units_count_astral_characters_differently() {
        let text = "a\u{1f600}";
        assert_eq!(LengthUnit::Chars.len(text), 2);
        assert_eq!(LengthUnit::Utf16.len(text), 3);
        assert_eq!(LengthUnit::Utf8Bytes.len(text), 5);
    }
}
