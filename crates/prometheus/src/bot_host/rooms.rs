//! The rules of a group chat discussion (spec: bot-mode, Group chats; Room
//! stop directives and held members): who is addressed, which words hold
//! a member, what a member turn is shown, and how a reply is read.

use std::fmt::Write as _;

/// Messages a member turn is shown at most.
pub const CONTEXT_MESSAGES: usize = 200;
/// Characters a member turn is shown at most.
pub const CONTEXT_CHARS: usize = 32_000;
/// The longest single message shown; a longer one is a marked excerpt.
pub const EXCERPT_CHARS: usize = 8_000;

/// The words that hold a member when next to its mention.
const STOP_WORDS: [&str; 3] = ["stop", "halt", "pause"];

/// `text` without code spans, fenced code, quoted passages and
/// blockquotes, where directives and mentions do not count.
fn prose(text: &str) -> String {
    let mut out = String::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.trim_start().starts_with('>') {
            continue;
        }
        let mut quoted = false;
        for c in line.chars() {
            if c == '`' || c == '"' {
                quoted = !quoted;
            } else if !quoted {
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || "@-_/".contains(c)))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Who a user message addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressed {
    /// `@all` or `@everyone`.
    pub everyone: bool,
    /// The members mentioned by `@<member>`.
    pub members: Vec<String>,
    /// The members a stop directive holds.
    pub held: Vec<String>,
}

/// What `text` addresses among `members` (stop directives only when
/// `detect`).
#[must_use]
pub fn addressed(text: &str, members: &[String], detect: bool) -> Addressed {
    let words = words(&prose(text));
    let mut found = Addressed {
        everyone: words.iter().any(|w| w == "@all" || w == "@everyone"),
        ..Addressed::default()
    };
    for (at, word) in words.iter().enumerate() {
        let Some(member) = members
            .iter()
            .find(|m| word.strip_prefix('@') == Some(m.to_lowercase().as_str()))
        else {
            continue;
        };
        let near = |offset: isize| {
            at.checked_add_signed(offset)
                .and_then(|n| words.get(n))
                .is_some_and(|w| STOP_WORDS.contains(&w.as_str()))
        };
        if detect && (near(1) || near(-1)) {
            found.held.push(member.clone());
        } else if !found.members.contains(member) {
            found.members.push(member.clone());
        }
    }
    found
}

/// One log message as a member turn shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// Who.
    pub author: String,
    /// What.
    pub text: String,
}

/// What member `member` of room `room` is shown: the messages since its
/// last turn (the newest that fit 200 messages and 32000 characters, a
/// longer single message cut to a marked 8000-character excerpt, the
/// count of the omitted ones) and how to answer.
#[must_use]
pub fn context(room: &str, member: &str, said: &[Said]) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut used = 0;
    for message in said.iter().rev() {
        let text = excerpt(&message.text);
        let line = format!("{}: {text}", message.author);
        let size = line.chars().count();
        if shown.len() == CONTEXT_MESSAGES || used + size > CONTEXT_CHARS {
            break;
        }
        used += size;
        shown.push(line);
    }
    let omitted = said.len() - shown.len();
    shown.reverse();
    let mut text = format!(
        "[Group chat \"{room}\": you are @{member}. Reply to the room in one message, or answer \
         [PASS] to stay silent this round. Write @user when you need the user.]\n"
    );
    if omitted > 0 {
        let _ = writeln!(text, "[{omitted} earlier messages omitted]");
    }
    text.push_str(&shown.join("\n"));
    text
}

fn excerpt(text: &str) -> String {
    match text.char_indices().nth(EXCERPT_CHARS) {
        Some((cut, _)) => format!(
            "{} [excerpt: message cut at {EXCERPT_CHARS} characters]",
            &text[..cut]
        ),
        None => text.to_owned(),
    }
}

/// Whether a member's reply passes (says nothing to the room).
#[must_use]
pub fn passes(reply: &str) -> bool {
    let reply = reply.trim();
    reply.is_empty() || ["[PASS]", "PASS", "[SILENT]", "NO_REPLY"].contains(&reply)
}

/// Whether a member's reply asks for the user.
#[must_use]
pub fn wants_user(reply: &str) -> bool {
    words(&prose(reply)).iter().any(|w| w == "@user")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Vec<String> {
        vec!["bot".into(), "builder".into(), "critic".into()]
    }

    /// Scenario: Stop word in prose.
    #[test]
    fn a_stop_word_holds_only_next_to_the_mention() {
        let prose = addressed("@bot the halt condition is x > 3", &members(), true);
        assert!(prose.held.is_empty());
        assert_eq!(prose.members, ["bot"]);
        assert_eq!(
            addressed("@builder stop", &members(), true).held,
            ["builder"]
        );
        assert_eq!(
            addressed("pause @critic please", &members(), true).held,
            ["critic"]
        );
        assert!(
            addressed("`@builder stop`", &members(), true)
                .held
                .is_empty()
        );
        assert!(
            addressed("> @builder stop", &members(), true)
                .held
                .is_empty()
        );
        assert!(
            addressed("@builder stop", &members(), false)
                .held
                .is_empty()
        );
        assert!(addressed("@all continue", &members(), true).everyone);
    }

    #[test]
    fn a_member_turn_shows_what_fits_and_counts_the_rest() {
        let said: Vec<Said> = (0..250)
            .map(|n| Said {
                author: "user".into(),
                text: format!("m{n}"),
            })
            .collect();
        let text = context("plans", "bot", &said);
        assert!(text.contains("[50 earlier messages omitted]"), "{text}");
        assert!(text.contains("user: m249") && !text.contains("user: m49\n"));
        let long = [Said {
            author: "critic".into(),
            text: "x".repeat(9_000),
        }];
        assert!(
            context("plans", "bot", &long).contains("[excerpt: message cut at 8000 characters]")
        );
        assert!(passes(" [PASS] ") && !passes("I agree"));
        assert!(wants_user("@user which one?") && !wants_user("ask the user"));
    }
}
