//! Cache-safe guidance injection (spec: mixture-of-agents, Cache-safe
//! guidance injection; Strict-alternation recovery). The references'
//! outputs reach the aggregator as one trailing private user message of
//! labelled guidance after the whole stable prefix; the session history
//! never keeps it, so each iteration's request is the previous one's
//! prefix with the old guidance replaced. Wires that merge adjacent user
//! turns get it as a separate content block of the last user message, so
//! the user's own block stays byte-identical; a destination that refuses
//! consecutive user messages gets it merged into the user's text.

use std::fmt::Write as _;

use pmt_config::schema::WireFormat;

use crate::request::{Message, Part, Request, Role};
use crate::resolve::Route;

/// Opens the guidance block (the aggregator and tests recognise it).
pub const OPEN: &str = "<moa-guidance>";
/// Closes the guidance block.
pub const CLOSE: &str = "</moa-guidance>";

/// One reference's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// `provider/model`.
    pub label: String,
    /// What it answered, or why it is unavailable.
    pub outcome: Result<String, String>,
}

/// The guidance text for `references`, each output passed through
/// `filter` (the `full` privacy filter, or nothing).
#[must_use]
pub fn text(references: &[Reference], filter: impl Fn(&str) -> String) -> String {
    let mut out = format!(
        "{OPEN}\nAdvisor models answered the conversation so far. Weigh their advice, \
         then answer or act yourself with the tools you have; the advisors cannot see \
         tool results and are not shown to the user as your reply.\n"
    );
    for (i, reference) in references.iter().enumerate() {
        let n = i + 1;
        let label = &reference.label;
        let _ = match &reference.outcome {
            Ok(text) => write!(out, "\n[advisor {n}: {label}]\n{}\n", filter(text.trim())),
            Err(why) => write!(
                out,
                "\n[advisor {n}: {label}] unavailable: {}\n",
                one_line(why)
            ),
        };
    }
    out.push_str(CLOSE);
    out
}

/// How the guidance joins the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A separate trailing user message.
    Split,
    /// A separate content block of the last user turn.
    Block,
    /// Merged into the last user message's text.
    Merged,
}

impl Shape {
    /// The shape for an aggregator on `route`; `merge` when that
    /// destination refused consecutive user messages before.
    #[must_use]
    pub fn of(route: &Route, merge: bool) -> Self {
        if merge {
            Self::Merged
        } else if route.wire_format == WireFormat::AnthropicMessages || route.bundled.is_some() {
            Self::Block
        } else {
            Self::Split
        }
    }
}

/// `request` with `guidance` after its whole prefix, in `shape`.
#[must_use]
pub fn inject(request: &Request, guidance: &str, shape: Shape) -> Request {
    let mut out = request.clone();
    let last = out.messages.last_mut();
    match (shape, last) {
        (Shape::Block, Some(last)) if last.role != Role::Assistant => {
            last.parts.push(Part::text(guidance));
        }
        (Shape::Merged, Some(last)) if last.role == Role::User => {
            match last.parts.iter_mut().rev().find_map(|p| match p {
                Part::Text { text } => Some(text),
                _ => None,
            }) {
                Some(text) => {
                    text.push_str("\n\n");
                    text.push_str(guidance);
                }
                None => last.parts.push(Part::text(guidance)),
            }
        }
        _ => out.messages.push(Message::text(Role::User, guidance)),
    }
    out
}

/// Whether `error` is a destination refusing consecutive user messages
/// (`Conversation roles must alternate`, HTTP 400).
#[must_use]
pub fn alternation_refused(error: &crate::classify::ProviderError) -> bool {
    let said = |text: &str| {
        let text = text.to_ascii_lowercase();
        text.contains("alternat") || text.contains("consecutive")
    };
    error.status == Some(400) && (said(&error.message) || error.body.as_deref().is_some_and(said))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            messages: vec![Message::text(Role::User, "plan it")],
            ..Request::default()
        }
    }

    #[test]
    fn guidance_trails_the_prefix_in_each_shape() {
        let split = inject(&request(), "G", Shape::Split);
        assert_eq!(split.messages.len(), 2);
        assert_eq!(
            split.messages[0],
            request().messages[0],
            "user turn untouched"
        );
        let block = inject(&request(), "G", Shape::Block);
        assert_eq!(block.messages.len(), 1);
        assert_eq!(
            block.messages[0].parts,
            [Part::text("plan it"), Part::text("G")]
        );
        let merged = inject(&request(), "G", Shape::Merged);
        assert_eq!(merged.messages[0].parts, [Part::text("plan it\n\nG")]);
    }

    #[test]
    fn failures_are_one_line_notes() {
        let text = text(
            &[
                Reference {
                    label: "a/x".into(),
                    outcome: Err("no credential\nfor a".into()),
                },
                Reference {
                    label: "b/y".into(),
                    outcome: Ok("use a queue".into()),
                },
            ],
            str::to_owned,
        );
        assert!(text.contains("[advisor 1: a/x] unavailable: no credential for a"));
        assert!(text.contains("[advisor 2: b/y]\nuse a queue"));
        assert!(text.starts_with(OPEN) && text.ends_with(CLOSE));
    }
}
