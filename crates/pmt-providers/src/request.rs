//! The internal model request (design D6): one type for every wire format,
//! with prompt-cache breakpoints as a first-class field. Wire adapters
//! translate it; nothing here is provider-specific, and the provider and
//! model identity never enter the system prompt text, so a route change
//! replays a byte-identical prefix (spec: Prompt caching and stable prefix
//! across route changes).

use pmt_config::schema::ReasoningLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One system prompt block; a cache breakpoint ends a cached tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemBlock {
    /// The text.
    pub text: String,
    /// The cached prefix ends after this block.
    #[serde(skip)]
    pub cache_breakpoint: bool,
}

/// Message author.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The user (and tool results, on Anthropic).
    User,
    /// The model.
    Assistant,
    /// Tool results.
    Tool,
}

/// A part of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image, inline (`data` is base64) or by URL.
    Image {
        /// MIME type (`image/png`).
        media_type: String,
        /// Base64 data, or an `https://` URL when `is_url`.
        data: String,
        /// `data` is a URL.
        is_url: bool,
    },
    /// A tool call the model made.
    ToolCall {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments as a JSON text.
        arguments: String,
    },
    /// The result of a tool call.
    ToolResult {
        /// The call id answered.
        call_id: String,
        /// Result text.
        content: String,
        /// The tool failed.
        is_error: bool,
    },
}

impl Part {
    /// A text part.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// One transcript message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Content parts.
    pub parts: Vec<Part>,
    /// The cached prefix ends after this message.
    #[serde(skip)]
    pub cache_breakpoint: bool,
}

impl Message {
    /// A message of one text part.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![Part::text(text)],
            cache_breakpoint: false,
        }
    }

    /// Whether the message carries an image.
    #[must_use]
    pub fn has_image(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Image { .. }))
    }
}

/// A tool schema offered to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

/// How the prompt cache is used for this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// No markers and no key (`prompt_cache.ttl = "off"`).
    Off,
    /// Five-minute markers (the default).
    #[default]
    FiveMinutes,
    /// One-hour markers.
    OneHour,
}

/// Prompt-cache settings carried by the request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CacheControl {
    /// Marker lifetime.
    pub ttl: CacheTtl,
    /// The stable key (`responses` / `chat_completions` providers that
    /// support one); see [`prompt_cache_key`].
    pub key: Option<String>,
    /// The tool list ends a cached tier.
    pub tools_breakpoint: bool,
}

impl CacheControl {
    /// The settings of a session under `prompt_cache.ttl` (`auto` uses
    /// five-minute markers): the session's stable key and a breakpoint
    /// after the tool list, unless caching is off.
    #[must_use]
    pub fn for_session(config: &pmt_config::schema::PromptCacheConfig, session_id: &str) -> Self {
        use pmt_config::schema::PromptCacheTtl;
        let ttl = match config.ttl {
            PromptCacheTtl::Off => {
                return Self {
                    ttl: CacheTtl::Off,
                    ..Self::default()
                };
            }
            PromptCacheTtl::OneHour => CacheTtl::OneHour,
            PromptCacheTtl::FiveMinutes | PromptCacheTtl::Auto => CacheTtl::FiveMinutes,
        };
        Self {
            ttl,
            key: Some(prompt_cache_key(session_id)),
            tools_breakpoint: true,
        }
    }
}

/// One model request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// Model id at the provider.
    pub model: String,
    /// System prompt blocks (frozen per session).
    pub system: Vec<SystemBlock>,
    /// Transcript.
    pub messages: Vec<Message>,
    /// Tool schemas.
    pub tools: Vec<Tool>,
    /// Output token cap.
    pub max_output_tokens: Option<u64>,
    /// Prompt-cache settings.
    pub cache: CacheControl,
    /// Reasoning effort to send (already clamped to the route; `None`
    /// sends no reasoning control). See [`crate::reasoning`].
    pub reasoning: Option<ReasoningLevel>,
}

impl Request {
    /// Whether any message carries an image.
    #[must_use]
    pub fn has_images(&self) -> bool {
        self.messages.iter().any(Message::has_image)
    }

    /// A rough token estimate (4 characters per token) of the prompt, for
    /// capability checks before the provider counts.
    #[must_use]
    pub fn estimated_tokens(&self) -> u64 {
        let chars: usize = self.system.iter().map(|b| b.text.len()).sum::<usize>()
            + self
                .messages
                .iter()
                .flat_map(|m| &m.parts)
                .map(part_len)
                .sum::<usize>()
            + self
                .tools
                .iter()
                .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
                .sum::<usize>();
        (chars as u64).div_ceil(4)
    }

    /// The content address of the request (design D10): SHA-256 over the
    /// canonical request sent to `provider`/`model` (the route's, which
    /// replace `self.model`), excluding transport fields, streaming flags
    /// and cache markers.
    ///
    /// # Panics
    ///
    /// Never: the canonical form holds only plain data.
    #[must_use]
    pub fn hash(&self, provider: &str, model: &str) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            provider: &'a str,
            model: &'a str,
            system: &'a [SystemBlock],
            messages: &'a [Message],
            tools: &'a [Tool],
            max_output_tokens: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            reasoning: Option<&'static str>,
        }
        let canonical = Canonical {
            provider,
            model,
            system: &self.system,
            messages: &self.messages,
            tools: &self.tools,
            max_output_tokens: self.max_output_tokens,
            reasoning: self.reasoning.map(ReasoningLevel::as_str),
        };
        let bytes = serde_json::to_vec(&canonical).expect("plain data serializes");
        format!("sha256:{}", hex(&Sha256::digest(bytes)))
    }
}

fn part_len(part: &Part) -> usize {
    match part {
        Part::Text { text } => text.len(),
        // Providers bill images by size class; a fixed share is enough here.
        Part::Image { .. } => 4 * 1_000,
        Part::ToolCall {
            name, arguments, ..
        } => name.len() + arguments.len(),
        Part::ToolResult { content, .. } => content.len(),
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The prompt cache key of a session: stable for its whole life (every
/// turn and route of the session sends the same key) and not the raw id.
#[must_use]
pub fn prompt_cache_key(session_id: &str) -> String {
    let digest = Sha256::digest(format!("prometheus-prompt-cache:{session_id}"));
    format!("pmt-{}", &hex(&digest)[..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_cache_markers_and_depends_on_content_and_route() {
        let mut a = Request {
            model: "m".into(),
            messages: vec![Message::text(Role::User, "hi")],
            ..Request::default()
        };
        let base = a.hash("p", "m");
        a.messages[0].cache_breakpoint = true;
        a.cache.key = Some("k".into());
        assert_eq!(a.hash("p", "m"), base);
        assert_ne!(a.hash("q", "m"), base);
        assert_ne!(a.hash("p", "n"), base);
        a.messages.push(Message::text(Role::Assistant, "yo"));
        assert_ne!(a.hash("p", "m"), base);
    }

    #[test]
    fn cache_key_is_stable_per_session_and_hides_the_id() {
        let key = prompt_cache_key("ses_01ABC");
        assert_eq!(key, prompt_cache_key("ses_01ABC"));
        assert_ne!(key, prompt_cache_key("ses_01ABD"));
        assert!(!key.contains("01ABC"));
    }
}
