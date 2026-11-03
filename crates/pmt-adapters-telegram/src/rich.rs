//! Telegram rich messages (spec: platform-adapters, Markup conversion per
//! platform): with `options.rich_messages = true` a reply goes out as the
//! agent's raw Markdown through Bot API 10.1 `sendRichMessage` (edits
//! through `editMessageText` with `rich_message`), up to 32768 characters,
//! so tables, task lists and the like render natively. A message over that
//! length, or one Telegram refuses, falls back to `MarkdownV2`; a Bot API
//! without the method turns rich messages off until the adapter restarts.

use pmt_gateway::messaging::SendError;
use serde_json::{Value, json};

/// The longest rich message, in UTF-16 code units.
pub(crate) const LIMIT: usize = 32_768;

/// Whether `text` fits one rich message.
pub(crate) fn fits(text: &str) -> bool {
    text.encode_utf16().count() <= LIMIT
}

/// The `rich_message` (`InputRichMessage`) of Markdown `text`.
pub(crate) fn payload(text: &str) -> Value {
    json!({"markdown": hard_breaks(text)})
}

/// Why a rich call failed in a way that falls back to `MarkdownV2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// This message was refused (its Markdown, its length).
    Message,
    /// The Bot API has no rich messages.
    Unavailable,
}

/// The refusal `err` is, or `None` for a failure that must not be resent
/// in another dialect (the request may have reached Telegram).
pub(crate) fn refusal(err: &SendError) -> Option<Refusal> {
    match err {
        SendError::Formatting(_) => Some(Refusal::Message),
        SendError::Permanent(why) => {
            let lower = why.to_ascii_lowercase();
            if why.starts_with("404 ")
                || lower.contains("method not found")
                || lower.contains("no such method")
            {
                Some(Refusal::Unavailable)
            } else {
                why.starts_with("400 ").then_some(Refusal::Message)
            }
        }
        _ => None,
    }
}

/// `text` with each lone newline (a Markdown soft break, which a rich
/// message would show as a space) made a hard break; blank lines, code
/// fences and pipe tables are left as written.
fn hard_breaks(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let kept = protected(&lines);
    let mut out = String::with_capacity(text.len() + lines.len());
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        let Some(next) = lines.get(i + 1) else {
            break;
        };
        let soft = !line.is_empty() && !next.is_empty() && !kept[i] && !kept[i + 1];
        out.push_str(if soft { "  \n" } else { "\n" });
    }
    out
}

/// Which lines belong to a code fence or a pipe table.
fn protected(lines: &[&str]) -> Vec<bool> {
    let mut kept = vec![false; lines.len()];
    let mut fence = false;
    let mut table = false;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            kept[i] = true;
            continue;
        }
        if !line.contains('|') {
            table = false;
        } else if lines.get(i + 1).is_some_and(|next| delimiter_row(next)) {
            table = true;
        }
        kept[i] = fence || table;
    }
    kept
}

/// A table's `|---|:--:|` row.
fn delimiter_row(line: &str) -> bool {
    let cells: Vec<&str> = line
        .trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect();
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let dashes = cell.trim_matches(':');
            !dashes.is_empty() && dashes.chars().all(|c| c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_newlines_become_hard_breaks_outside_fences_and_tables() {
        let text = "Line one\nline two\n\nNext paragraph\n```\na\nb\n```\n| a | b |\n|---|---|\n| 1 | 2 |\nafter";
        assert_eq!(
            hard_breaks(text),
            "Line one  \nline two\n\nNext paragraph\n```\na\nb\n```\n| a | b |\n|---|---|\n| 1 | 2 |\nafter"
        );
    }

    #[test]
    fn refusals_fall_back_and_transient_failures_do_not() {
        assert_eq!(
            refusal(&SendError::Permanent(
                "400 Bad Request: can't parse rich message".into()
            )),
            Some(Refusal::Message)
        );
        assert_eq!(
            refusal(&SendError::Permanent(
                "404 Not Found: method not found".into()
            )),
            Some(Refusal::Unavailable)
        );
        assert_eq!(
            refusal(&SendError::Formatting("can't parse entities".into())),
            Some(Refusal::Message)
        );
        assert_eq!(refusal(&SendError::Ambiguous("timeout".into())), None);
        assert_eq!(refusal(&SendError::Permanent("403 Forbidden".into())), None);
        assert!(fits(&"x".repeat(LIMIT)));
        assert!(!fits(&"x".repeat(LIMIT + 1)));
    }
}
