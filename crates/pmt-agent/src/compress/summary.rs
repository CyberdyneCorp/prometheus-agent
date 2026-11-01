//! The handoff summary (spec: context-compression, Summary budget; Handoff
//! summary shape; Iterative update; Summary failure handling).
//!
//! The summarizer (the auxiliary `compression` route, [`AuxSummarizer`])
//! gets the pruned middle as a transcript, the previous summary as the
//! document to update, the user's focus text and an output cap no lower
//! than the budget. Whatever it writes is normalised into the fixed shape
//! (header, the required sections in order, the end marker), the
//! `## Open Todos` section always carrying the open items of the `todo`
//! list verbatim. When no summary can be had, [`fallback`] builds one
//! deterministically from the previous summary, the prune lines and the
//! items extracted from the region.

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_core::error::{Error, ErrorKind, codes};
use pmt_providers::auxiliary::{AuxCaller, AuxRoute};
use pmt_providers::cost::CallContext;
use pmt_providers::event::{StopReason, StreamEvent};
use pmt_providers::request::{Message, Request, Role as WireRole, SystemBlock};
use pmt_store::{Message as Row, Role};
use serde_json::Value;

use crate::turn::model::BoxFuture;

pub use super::items::{Items, fallback};

/// The first line of every summary row.
pub const HEADER: &str = "[CONTEXT COMPACTION - REFERENCE ONLY]";
/// The last line of every summary row.
pub const FOOTER: &str =
    "--- END OF CONTEXT SUMMARY - respond to the message below, not the summary above ---";
/// The required sections, in order (`Pinned` is added when pins exist, M2).
pub const SECTIONS: [&str; 6] = [
    "Goal",
    "Progress",
    "Decisions",
    "Files",
    "Open Todos",
    "Next Steps",
];
/// What an empty section says.
const EMPTY: &str = "(none recorded)";

const SYSTEM: &str = "You compact the conversation of another AI agent so it can continue \
its work after the older messages are removed. Write a factual handoff summary in Markdown \
with exactly these sections, in this order: ## Goal, ## Progress, ## Decisions, ## Files, \
## Open Todos, ## Next Steps. Keep file paths, commands, identifiers, numbers, errors and \
the user's exact requirements; cite pruned tool output by its ref; never invent anything. \
Do not answer the conversation and do not call tools.";

/// Why no summary came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Any other failure.
    Failed,
    /// No answer within `compression.summary_timeout_secs`.
    Timeout,
    /// The answer hit the output cap.
    Truncated,
    /// The credential was rejected (aborts: no fallback).
    Auth,
    /// Payment or quota exhausted (aborts: no fallback).
    Quota,
}

impl FailureKind {
    /// Auth and quota failures abort the compaction instead of committing
    /// the fallback summary.
    #[must_use]
    pub const fn aborts(self) -> bool {
        matches!(self, Self::Auth | Self::Quota)
    }

    /// The `compression.failed` reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "summary_failed",
            Self::Timeout => "summary_timeout",
            Self::Truncated => "summary_truncated",
            Self::Auth => "summary_auth",
            Self::Quota => "summary_quota",
        }
    }
}

/// A summary that could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryFailure {
    /// The class.
    pub kind: FailureKind,
    /// What happened.
    pub message: String,
}

impl SummaryFailure {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Writes summaries (the auxiliary `compression` route).
pub trait Summarizer: Send + Sync {
    /// Sends `request` and returns the summary text.
    fn summarize<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<String, SummaryFailure>>;

    /// The compression model's context window, when known (it lowers the
    /// trigger when smaller than the main model's).
    fn context_window(&self) -> Option<u64> {
        None
    }
}

/// What the summarizer is asked.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The pruned middle, rendered.
    pub transcript: String,
    /// The previous summary's body (the document to update).
    pub previous: Option<String>,
    /// `/compress <focus>`.
    pub focus: Option<String>,
    /// The summary budget in tokens.
    pub budget: u64,
}

/// The summary request of `input`; its output cap is the budget.
#[must_use]
pub fn request(input: &Input) -> Request {
    let mut text = String::new();
    if let Some(previous) = &input.previous {
        text.push_str(
            "PREVIOUS SUMMARY - update this document with the conversation below; keep what \
             is still true, drop what is obsolete:\n\n",
        );
        text.push_str(previous);
        text.push_str("\n\n");
    }
    text.push_str("CONVERSATION TO SUMMARIZE:\n\n");
    text.push_str(&input.transcript);
    if let Some(focus) = &input.focus {
        text.push_str("\n\nPRIORITY FROM THE USER - preserve everything about this first: ");
        text.push_str(focus);
    }
    let _ = write!(
        text,
        "

Write the summary now, in at most {} tokens.",
        input.budget
    );
    Request {
        system: vec![SystemBlock {
            text: SYSTEM.to_owned(),
            cache_breakpoint: false,
        }],
        messages: vec![Message::text(WireRole::User, text)],
        max_output_tokens: Some(input.budget),
        ..Request::default()
    }
}

/// `rows` as the transcript the summarizer reads.
#[must_use]
pub fn render(rows: &[Row]) -> String {
    let mut out = Vec::new();
    for row in rows {
        let text = row.content.as_deref().unwrap_or_default().trim();
        match row.role {
            Role::User if !text.is_empty() => out.push(format!("[USER] {text}")),
            Role::Assistant => {
                if !text.is_empty() {
                    out.push(format!("[ASSISTANT] {text}"));
                }
                for call in calls(row) {
                    out.push(format!("[ACTION] {}({})", call.0, call.1));
                }
            }
            Role::Tool => out.push(format!(
                "[OBSERVATION {}] {text}",
                row.tool_name.as_deref().unwrap_or("tool")
            )),
            Role::User | Role::System => {}
        }
    }
    out.join("\n")
}

/// `(name, arguments)` of each call of an assistant row.
pub(crate) fn calls(row: &Row) -> Vec<(String, String)> {
    row.tool_calls
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|call| {
            let field = |key: &str| {
                call.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            (field("name"), field("arguments"))
        })
        .collect()
}

/// Ordered `## Title` sections of a Markdown summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sections {
    entries: Vec<(String, String)>,
}

impl Sections {
    /// Parses `text`; text before the first heading goes to `Progress`,
    /// and an echoed header or end marker is dropped.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut sections = Self::default();
        let mut title = "Progress".to_owned();
        let mut body = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == HEADER || trimmed == FOOTER {
                continue;
            }
            if let Some(heading) = trimmed.strip_prefix("## ") {
                sections.append(&title, &body);
                title = canonical(heading);
                body.clear();
                continue;
            }
            body.push_str(line);
            body.push('\n');
        }
        sections.append(&title, &body);
        sections
    }

    /// The text of section `title`.
    #[must_use]
    pub fn get(&self, title: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(t, _)| t == title)
            .map(|(_, body)| body.as_str())
    }

    /// Appends `text` to section `title` (created when missing).
    pub fn append(&mut self, title: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match self.entries.iter_mut().find(|(t, _)| t == title) {
            Some((_, body)) => {
                body.push('\n');
                body.push_str(text);
            }
            None => self.entries.push((title.to_owned(), text.to_owned())),
        }
    }

    /// The summary row text: header, the required sections in order (Open
    /// Todos from `open_todos` when the list has open items), any other
    /// section before `Next Steps`, and the end marker.
    #[must_use]
    pub fn document(&self, open_todos: Option<&str>) -> String {
        let mut out = format!("{HEADER}\n");
        let mut section = |title: &str, body: &str| {
            let _ = write!(out, "\n## {title}\n{}\n", body.trim());
        };
        for title in &SECTIONS[..5] {
            let body = match (*title, open_todos) {
                ("Open Todos", Some(todos)) => todos,
                _ => self.get(title).unwrap_or(EMPTY),
            };
            section(title, body);
        }
        for (title, body) in &self.entries {
            if !SECTIONS.contains(&title.as_str()) {
                section(title, body);
            }
        }
        section("Next Steps", self.get("Next Steps").unwrap_or(EMPTY));
        out.push('\n');
        out.push_str(FOOTER);
        out
    }
}

/// A heading as its canonical required title when it names one.
fn canonical(heading: &str) -> String {
    let heading = heading.trim();
    SECTIONS
        .iter()
        .find(|s| s.eq_ignore_ascii_case(heading))
        .map_or_else(|| heading.to_owned(), |s| (*s).to_owned())
}

/// The body of a stored summary (the text between header and end marker).
#[must_use]
pub fn body_of(text: &str) -> String {
    text.lines()
        .filter(|l| l.trim() != HEADER && l.trim() != FOOTER)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The `compression` auxiliary route as the summarizer.
pub struct AuxSummarizer {
    caller: Arc<AuxCaller>,
    route: AuxRoute,
    context: CallContext,
    window: Option<u64>,
}

impl AuxSummarizer {
    /// Calls `route` (resolved for `AuxTask::Compression`) through
    /// `caller` for the session of `context`; `window` is the route
    /// model's context window when known.
    #[must_use]
    pub fn new(
        caller: Arc<AuxCaller>,
        route: AuxRoute,
        context: CallContext,
        window: Option<u64>,
    ) -> Self {
        Self {
            caller,
            route,
            context,
            window,
        }
    }
}

impl Summarizer for AuxSummarizer {
    fn summarize<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<String, SummaryFailure>> {
        Box::pin(async move {
            let reply = self
                .caller
                .call(&self.route, request, &self.context)
                .await
                .map_err(|err| classify(&err))?;
            let truncated = reply.events.iter().any(|e| {
                matches!(
                    e,
                    StreamEvent::Stop {
                        stop_reason: StopReason::MaxTokens
                    }
                )
            });
            if truncated {
                return Err(SummaryFailure::new(
                    FailureKind::Truncated,
                    "the summary hit its output cap",
                ));
            }
            Ok(reply.text)
        })
    }

    fn context_window(&self) -> Option<u64> {
        self.window
    }
}

/// The failure class of an auxiliary call error.
#[must_use]
pub fn classify(err: &Error) -> SummaryFailure {
    let message = err.message().to_owned();
    let kind = if err.kind() == ErrorKind::Auth || err.code() == &codes::PROVIDER_AUTH_FAILED {
        FailureKind::Auth
    } else if message.contains(": billing error") {
        FailureKind::Quota
    } else if message.contains(": timeout error") || message.contains("did not finish within") {
        FailureKind::Timeout
    } else {
        FailureKind::Failed
    };
    SummaryFailure { kind, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_has_the_fixed_shape_whatever_the_model_wrote() {
        let model = format!(
            "{HEADER}\nSome preamble.\n## goal\nShip it.\n## Files\n- src/a.rs\n## Risks\nNone.\n## Next steps\nTest."
        );
        let doc = Sections::parse(&model).document(Some("- [pending] write docs (t1)"));
        let headings: Vec<&str> = doc.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            headings,
            [
                "## Goal",
                "## Progress",
                "## Decisions",
                "## Files",
                "## Open Todos",
                "## Risks",
                "## Next Steps"
            ]
        );
        assert!(doc.starts_with(HEADER));
        assert!(doc.ends_with(FOOTER));
        assert_eq!(doc.matches(HEADER).count(), 1);
        assert!(doc.contains("## Progress\nSome preamble."));
        assert!(doc.contains("## Decisions\n(none recorded)"));
        assert!(doc.contains("## Open Todos\n- [pending] write docs (t1)"));
        assert_eq!(body_of(&doc).lines().next(), Some("## Goal"));
    }

    #[test]
    fn the_request_carries_the_previous_summary_focus_and_cap() {
        let req = request(&Input {
            transcript: "[USER] hi".into(),
            previous: Some("## Goal\nS1".into()),
            focus: Some("the auth bug".into()),
            budget: 4_000,
        });
        let text = match &req.messages[0].parts[0] {
            pmt_providers::request::Part::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("PREVIOUS SUMMARY"));
        assert!(text.contains("## Goal\nS1"));
        assert!(text.contains("the auth bug"));
        assert_eq!(req.max_output_tokens, Some(4_000));
        assert!(req.tools.is_empty());
    }

    #[test]
    fn auth_and_quota_failures_abort() {
        let auth = Error::new(&codes::PROVIDER_AUTH_FAILED, "acme: auth error: bad key");
        assert_eq!(classify(&auth).kind, FailureKind::Auth);
        let quota = Error::new(
            &codes::PROVIDER_CALL_FAILED,
            "acme: billing error: no credit",
        );
        assert_eq!(classify(&quota).kind, FailureKind::Quota);
        let slow = Error::new(
            &codes::PROVIDER_CALL_FAILED,
            "auxiliary task 'compression' did not finish within 120 s",
        );
        assert_eq!(classify(&slow).kind, FailureKind::Timeout);
        assert!(FailureKind::Auth.aborts() && FailureKind::Quota.aborts());
        assert!(!FailureKind::Timeout.aborts() && !FailureKind::Truncated.aborts());
    }
}
