//! The mock script: ordered rules, each a set of matchers and what to
//! answer (text, reasoning, tool calls) or which fault to inject (spec:
//! replay-and-evaluation, Deterministic mock provider; Mock fault
//! injection).
//!
//! ```toml
//! latency_ms = 0                 # default delay before every chunk
//!
//! [[rule]]
//! user = "summarise"             # or user_regex; tool_result(_regex)
//! [[rule.tool_calls]]
//! name = "read_file"
//! arguments = { path = "README.md" }
//!
//! [[rule]]
//! text = "done"
//! ```

use std::time::Duration;

use pmt_core::error::{Error, codes};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

/// A parsed, validated mock script.
#[derive(Debug, Clone)]
pub struct Script {
    pub(crate) rules: Vec<Rule>,
}

/// One rule: when it applies and what it answers.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub matcher: Matcher,
    pub answer: Answer,
    pub uses: Uses,
    pub latency: Duration,
}

/// How many calls a rule answers before later rules take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Uses {
    Times(u32),
    Always,
}

impl Uses {
    pub(crate) fn available(self, used: u32) -> bool {
        match self {
            Self::Times(times) => used < times,
            Self::Always => true,
        }
    }
}

/// Conditions that must all hold for a rule to apply.
#[derive(Debug, Clone, Default)]
pub(crate) struct Matcher {
    pub step: Option<usize>,
    pub model: Option<String>,
    pub user: Option<TextMatch>,
    pub tool_result: Option<TextMatch>,
}

/// A substring or a regex.
#[derive(Debug, Clone)]
pub(crate) enum TextMatch {
    Contains(String),
    Regex(Regex),
}

impl TextMatch {
    pub(crate) fn matches(&self, text: &str) -> bool {
        match self {
            Self::Contains(needle) => text.contains(needle.as_str()),
            Self::Regex(regex) => regex.is_match(text),
        }
    }
}

/// What a rule answers.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// A streamed response, possibly broken by a stream fault after its
    /// content.
    Respond(Response),
    /// A failure before anything is streamed.
    Fail(Fault),
}

/// A streamed response, in stream order: reasoning, text, tool calls.
#[derive(Debug, Clone, Default)]
pub(crate) struct Response {
    pub reasoning: Option<String>,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub broken: Option<StreamFault>,
}

/// A scripted tool call; `arguments` is the JSON text streamed.
#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: String,
}

/// A fault that fails the request before any event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    RateLimit { retry_after_secs: Option<u64> },
    Timeout,
    ServerError { status: u16 },
    AuthError,
    ContextOverflow,
}

/// A fault that breaks the stream after the rule's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamFault {
    Malformed,
    Truncated,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScript {
    #[serde(default)]
    latency_ms: u64,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    step: Option<usize>,
    model: Option<String>,
    user: Option<String>,
    user_regex: Option<String>,
    tool_result: Option<String>,
    tool_result_regex: Option<String>,
    text: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
    fault: Option<FaultKind>,
    retry_after_secs: Option<u64>,
    status: Option<u16>,
    times: Option<u32>,
    #[serde(default)]
    repeat: bool,
    latency_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolCall {
    id: Option<String>,
    name: String,
    arguments: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FaultKind {
    RateLimit,
    Timeout,
    ServerError,
    AuthError,
    ContextOverflow,
    MalformedStream,
    TruncatedStream,
}

impl FaultKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Timeout => "timeout",
            Self::ServerError => "server_error",
            Self::AuthError => "auth_error",
            Self::ContextOverflow => "context_overflow",
            Self::MalformedStream => "malformed_stream",
            Self::TruncatedStream => "truncated_stream",
        }
    }
}

impl Script {
    /// Parses `text`; `origin` (the file) prefixes every error.
    ///
    /// # Errors
    ///
    /// `mock.script_invalid` for malformed TOML, an unknown field, or an
    /// invalid rule (named by its 1-based position).
    pub fn parse(text: &str, origin: &str) -> Result<Self, Error> {
        let invalid = |message: String| Error::new(&codes::MOCK_SCRIPT_INVALID, message);
        let raw: RawScript = toml::from_str(text).map_err(|e| invalid(format!("{origin}: {e}")))?;
        let rules = raw
            .rule
            .into_iter()
            .enumerate()
            .map(|(i, rule)| {
                Rule::parse(rule, raw.latency_ms)
                    .map_err(|why| invalid(format!("{origin}: rule {}: {why}", i + 1)))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { rules })
    }

    /// Reads and parses the script at `path`.
    ///
    /// # Errors
    ///
    /// `mock.script_missing` when it cannot be read, else as
    /// [`parse`](Self::parse).
    pub fn load(path: &std::path::Path) -> Result<Self, Error> {
        let origin = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                &codes::MOCK_SCRIPT_MISSING,
                format!("cannot read mock script {origin}: {e}"),
            )
        })?;
        Self::parse(&text, &origin)
    }
}

impl Rule {
    fn parse(raw: RawRule, default_latency_ms: u64) -> Result<Self, String> {
        let uses = match (raw.times, raw.repeat) {
            (Some(_), true) => return Err("times and repeat are exclusive".into()),
            (Some(0), false) => return Err("times must be at least 1".into()),
            (Some(times), false) => Uses::Times(times),
            (None, true) => Uses::Always,
            (None, false) => Uses::Times(1),
        };
        let latency = Duration::from_millis(raw.latency_ms.unwrap_or(default_latency_ms));
        Ok(Self {
            matcher: matcher(&raw)?,
            answer: answer(raw)?,
            uses,
            latency,
        })
    }
}

fn matcher(raw: &RawRule) -> Result<Matcher, String> {
    Ok(Matcher {
        step: raw.step,
        model: raw.model.clone(),
        user: text_match("user", raw.user.as_deref(), raw.user_regex.as_deref())?,
        tool_result: text_match(
            "tool_result",
            raw.tool_result.as_deref(),
            raw.tool_result_regex.as_deref(),
        )?,
    })
}

fn text_match(
    field: &str,
    contains: Option<&str>,
    regex: Option<&str>,
) -> Result<Option<TextMatch>, String> {
    match (contains, regex) {
        (Some(_), Some(_)) => Err(format!("{field} and {field}_regex are exclusive")),
        (Some(needle), None) => Ok(Some(TextMatch::Contains(needle.to_owned()))),
        (None, Some(pattern)) => Regex::new(pattern)
            .map(|r| Some(TextMatch::Regex(r)))
            .map_err(|e| format!("{field}_regex: {e}")),
        (None, None) => Ok(None),
    }
}

fn answer(raw: RawRule) -> Result<Answer, String> {
    let kind = raw.fault;
    if raw.retry_after_secs.is_some() && kind != Some(FaultKind::RateLimit) {
        return Err("retry_after_secs applies only to fault = \"rate_limit\"".into());
    }
    if raw.status.is_some() && kind != Some(FaultKind::ServerError) {
        return Err("status applies only to fault = \"server_error\"".into());
    }
    let response = response(raw.reasoning, raw.text, raw.tool_calls)?;
    let has_content =
        response.reasoning.is_some() || response.text.is_some() || !response.tool_calls.is_empty();
    let fault = match kind {
        None if has_content => return Ok(Answer::Respond(response)),
        None => return Err("answers nothing: set text, reasoning, tool_calls or fault".into()),
        Some(FaultKind::MalformedStream) => StreamFault::Malformed,
        Some(FaultKind::TruncatedStream) => StreamFault::Truncated,
        Some(kind) if has_content => {
            return Err(format!(
                "fault \"{}\" fails before anything is streamed and takes no text, reasoning or tool_calls",
                kind.as_str()
            ));
        }
        Some(FaultKind::RateLimit) => {
            return Ok(Answer::Fail(Fault::RateLimit {
                retry_after_secs: raw.retry_after_secs,
            }));
        }
        Some(FaultKind::Timeout) => return Ok(Answer::Fail(Fault::Timeout)),
        Some(FaultKind::AuthError) => return Ok(Answer::Fail(Fault::AuthError)),
        Some(FaultKind::ContextOverflow) => return Ok(Answer::Fail(Fault::ContextOverflow)),
        Some(FaultKind::ServerError) => return server_error(raw.status).map(Answer::Fail),
    };
    Ok(Answer::Respond(Response {
        broken: Some(fault),
        ..response
    }))
}

fn server_error(status: Option<u16>) -> Result<Fault, String> {
    let status = status.unwrap_or(500);
    if (500..=599).contains(&status) {
        Ok(Fault::ServerError { status })
    } else {
        Err(format!("status {status} is not a server error (500-599)"))
    }
}

fn response(
    reasoning: Option<String>,
    text: Option<String>,
    calls: Vec<RawToolCall>,
) -> Result<Response, String> {
    let tool_calls = calls
        .into_iter()
        .map(|call| {
            if call.name.trim().is_empty() {
                return Err("tool_calls: name must not be empty".to_owned());
            }
            let arguments = match call.arguments {
                None => "{}".to_owned(),
                // A string is the raw argument text (it may be invalid JSON).
                Some(Value::String(text)) => text,
                Some(value) => value.to_string(),
            };
            Ok(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(Response {
        reasoning,
        text,
        tool_calls,
        broken: None,
    })
}
