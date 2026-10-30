//! The typed tool result contract (spec: tool-system, Typed tool result
//! contract).
//!
//! Every dispatched call ends in one [`ToolResult`]: a [`Status`], content
//! as text and/or media parts, and free-form metadata. Handler failures and
//! panics become `status = "error"` results, never errors of the turn.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The tool did its job.
    Ok,
    /// The tool or the pipeline failed.
    Error,
    /// A hook, deny rule or hardline pattern stopped the call.
    Blocked,
    /// A capability check or approval refused the call.
    Denied,
    /// The tool ran with reduced function.
    Degraded,
    /// The call exceeded its timeout.
    Timeout,
    /// The user interrupted the call.
    Cancelled,
}

impl Status {
    /// The wire string (`ok`, `error`...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Blocked => "blocked",
            Self::Denied => "denied",
            Self::Degraded => "degraded",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the call did its job (fully or degraded).
    #[must_use]
    pub fn succeeded(self) -> bool {
        matches!(self, Self::Ok | Self::Degraded)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One content part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// A media file (image, audio...) stored by the tool.
    Media {
        /// MIME type.
        mime: String,
        /// Where the file is.
        path: String,
    },
}

/// The result of one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// How the call ended.
    pub status: Status,
    /// Text and media parts, in order.
    pub content: Vec<Content>,
    /// Tool-specific metadata.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

impl ToolResult {
    /// A result of `status` holding one text part.
    #[must_use]
    pub fn text(status: Status, text: impl Into<String>) -> Self {
        Self {
            status,
            content: vec![Content::Text { text: text.into() }],
            metadata: Map::new(),
        }
    }

    /// An `ok` result holding `text`.
    #[must_use]
    pub fn ok(text: impl Into<String>) -> Self {
        Self::text(Status::Ok, text)
    }

    /// An `error` result holding `text`.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self::text(Status::Error, text)
    }

    /// The text parts joined by newlines.
    #[must_use]
    pub fn text_content(&self) -> String {
        let texts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|part| match part {
                Content::Text { text } => Some(text.as_str()),
                Content::Media { .. } => None,
            })
            .collect();
        texts.join("\n")
    }

    /// Characters of text content (what the result budget measures).
    #[must_use]
    pub fn text_chars(&self) -> usize {
        self.content
            .iter()
            .map(|part| match part {
                Content::Text { text } => text.chars().count(),
                Content::Media { .. } => 0,
            })
            .sum()
    }

    /// Replaces every text part with one `text` part placed where the
    /// first text part was; media parts keep their order.
    pub fn replace_text(&mut self, text: String) {
        let mut replacement = Some(Content::Text { text });
        let mut parts = Vec::with_capacity(self.content.len());
        for part in self.content.drain(..) {
            match part {
                Content::Text { .. } => parts.extend(replacement.take()),
                media @ Content::Media { .. } => parts.push(media),
            }
        }
        parts.extend(replacement);
        self.content = parts;
    }
}

#[cfg(test)]
mod tests {
    use super::{Content, Status, ToolResult};

    #[test]
    fn replace_text_keeps_media_in_place() {
        let mut result = ToolResult {
            status: Status::Ok,
            content: vec![
                Content::Media {
                    mime: "image/png".into(),
                    path: "a.png".into(),
                },
                Content::Text { text: "one".into() },
                Content::Text { text: "two".into() },
            ],
            metadata: serde_json::Map::new(),
        };
        assert_eq!(result.text_content(), "one\ntwo");
        assert_eq!(result.text_chars(), 6);
        result.replace_text("short".into());
        assert_eq!(result.content.len(), 2);
        assert!(matches!(result.content[0], Content::Media { .. }));
        assert_eq!(result.text_content(), "short");
    }

    #[test]
    fn status_serializes_as_snake_case() {
        let json = serde_json::to_string(&ToolResult::text(Status::Cancelled, "x")).unwrap();
        assert!(json.contains(r#""status":"cancelled""#), "{json}");
    }
}
