//! The conversation recap printed when an interactive session is resumed
//! (spec: cli, Conversation recap on resume). It is local output only:
//! nothing of it reaches the model.

use std::fmt::Write as _;

use pmt_config::schema::{DisplayConfig, ResumeDisplay};
use pmt_store::{Message, Role, Session};

/// One exchange: what the user said and the assistant's text replies.
struct Exchange<'a> {
    user: String,
    replies: Vec<&'a str>,
}

/// The recap of `session` (whose active messages are `messages`) under
/// `display`; `None` for `resume_display = "off"`.
#[must_use]
pub fn render(session: &Session, messages: &[Message], display: &DisplayConfig) -> Option<String> {
    match display.resume_display {
        ResumeDisplay::Off => None,
        ResumeDisplay::Minimal => Some(minimal(session, messages)),
        ResumeDisplay::Full => Some(full(session, messages, display)),
    }
}

fn minimal(session: &Session, messages: &[Message]) -> String {
    format!(
        "Resumed {} — {} messages, last activity {}",
        title(session),
        messages.len(),
        session.last_activity_at.strftime("%Y-%m-%d %H:%M UTC")
    )
}

fn title(session: &Session) -> String {
    session
        .title
        .as_deref()
        .map_or_else(|| session.id.to_string(), |t| format!("'{t}'"))
}

fn full(session: &Session, messages: &[Message], display: &DisplayConfig) -> String {
    let exchanges = exchanges(messages);
    let keep = usize::try_from(display.resume_exchanges).unwrap_or(usize::MAX);
    let shown = &exchanges[exchanges.len().saturating_sub(keep)..];
    let mut text = format!("{}\n", minimal(session, messages));
    let last = shown.len().saturating_sub(1);
    for (index, exchange) in shown.iter().enumerate() {
        let user = clamp_chars(&exchange.user, display.resume_max_user_chars);
        let _ = writeln!(text, "> {}", user.replace('\n', "\n  "));
        let final_reply = exchange.replies.len().saturating_sub(1);
        for (n, reply) in exchange.replies.iter().enumerate() {
            let body = if index == last && n == final_reply {
                (*reply).to_owned()
            } else {
                clamp_reply(reply, display)
            };
            let _ = writeln!(text, "  {}", body.replace('\n', "\n  "));
        }
    }
    text
}

/// User turns and the assistant text that answered them. Tool rows,
/// tool-only assistant messages and notices the system wrote are skipped.
fn exchanges(messages: &[Message]) -> Vec<Exchange<'_>> {
    let mut out: Vec<Exchange<'_>> = Vec::new();
    for message in messages {
        match message.role {
            Role::User if !authored(message) => out.push(Exchange {
                user: user_text(message),
                replies: Vec::new(),
            }),
            Role::Assistant => {
                let text = message.content.as_deref().unwrap_or_default().trim();
                if let (false, Some(exchange)) = (text.is_empty(), out.last_mut()) {
                    exchange.replies.push(text);
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether Prometheus (not the user) wrote this user-role row.
fn authored(message: &Message) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get(pmt_agent::turn::transcript::AUTHORED))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// What the user typed (without the additions the wire text carries).
fn user_text(message: &Message) -> String {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get("user_text"))
        .and_then(serde_json::Value::as_str)
        .or(message.content.as_deref())
        .unwrap_or_default()
        .to_owned()
}

fn clamp_reply(text: &str, display: &DisplayConfig) -> String {
    let lines = usize::try_from(display.resume_max_assistant_lines).unwrap_or(usize::MAX);
    let kept: Vec<&str> = text.lines().take(lines).collect();
    let cut_lines = text.lines().count() > kept.len();
    let joined = kept.join("\n");
    let clamped = clamp_chars(&joined, display.resume_max_assistant_chars);
    if cut_lines && !clamped.ends_with('…') {
        format!("{clamped}…")
    } else {
        clamped
    }
}

fn clamp_chars(text: &str, max: u32) -> String {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_clamped_by_characters_and_lines() {
        let display = DisplayConfig {
            resume_max_assistant_chars: 10,
            resume_max_assistant_lines: 2,
            ..DisplayConfig::default()
        };
        assert_eq!(clamp_reply("a\nb\nc", &display), "a\nb…");
        assert_eq!(clamp_reply("0123456789abc", &display), "0123456789…");
        assert_eq!(clamp_chars("héllo", 3), "hél…");
    }
}
