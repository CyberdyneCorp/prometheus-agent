//! The card model approval and clarify prompts render through (spec:
//! platform-adapters, Interactive approval cards on every platform): a
//! title, the details (exact action, working directory, risk reasons or
//! the question), the expiry time and the choices. On `native` adapters
//! each choice is a button whose callback data is
//! `<kind>:<id>:<choice>:<mac>` (`appr` for approvals, `clar` for clarify
//! choices), where `<mac>` is an HMAC-SHA256 over kind, request id and
//! choice under a key drawn at every daemon start, so a forged or
//! replayed-across-restarts callback never decides anything. The same
//! choices render as a numbered list (`numbered` adapters) or as
//! `/approve <id> <choice>` instructions (`none`).

use std::fmt::Write as _;

use jiff::{SignedDuration, Zoned};

use super::descriptor::Interactive;
use super::sign::{constant_time_eq, hmac_sha256};

/// The approval choices, in card order.
pub(crate) const CHOICES: [&str; 4] = ["once", "session", "always", "deny"];

/// Hex characters of the MAC kept in callback data (128 bits).
const MAC_HEX: usize = 32;

/// Characters of a button label (Discord's limit; Telegram shows less).
const LABEL_CHARS: usize = 80;

/// What a card asks; its callback data prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardKind {
    /// An approval (`appr`): choices are [`CHOICES`].
    Approval,
    /// A clarify question (`clar`): the choice is `<question>.<number>`.
    Clarify,
    /// A paged listing's previous/next button (`page`): the id names the
    /// listing, the choice is the page to show.
    Page,
    /// A gateway confirmation (`conf`: `/restart`, `/update`): the choice
    /// is `yes` or `no`.
    Confirm,
}

impl CardKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Approval => "appr",
            Self::Clarify => "clar",
            Self::Page => "page",
            Self::Confirm => "conf",
        }
    }

    fn of_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "appr" => Some(Self::Approval),
            "clar" => Some(Self::Clarify),
            "page" => Some(Self::Page),
            "conf" => Some(Self::Confirm),
            _ => None,
        }
    }
}

/// The per-boot key.
pub(crate) struct CardKey([u8; 32]);

impl std::fmt::Debug for CardKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CardKey(..)")
    }
}

impl CardKey {
    /// A fresh random key.
    pub(crate) fn random() -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS random source works");
        Self(key)
    }

    /// The callback data of `choice` on request `id`.
    pub(crate) fn data(&self, kind: CardKind, id: &str, choice: &str) -> String {
        let prefix = kind.prefix();
        format!("{prefix}:{id}:{choice}:{}", self.mac(kind, id, choice))
    }

    /// The kind, request id and choice of valid callback data; `None` for
    /// anything malformed or with a MAC that does not verify.
    pub(crate) fn verify<'a>(&self, data: &'a str) -> Option<(CardKind, &'a str, &'a str)> {
        let (prefix, rest) = data.split_once(':')?;
        let kind = CardKind::of_prefix(prefix)?;
        let (rest, mac) = rest.rsplit_once(':')?;
        let (id, choice) = rest.rsplit_once(':')?;
        let expected = self.mac(kind, id, choice);
        constant_time_eq(expected.as_bytes(), mac.as_bytes()).then_some((kind, id, choice))
    }

    fn mac(&self, kind: CardKind, id: &str, choice: &str) -> String {
        let message = format!("{}\u{1f}{id}\u{1f}{choice}", kind.prefix());
        let digest = hmac_sha256(&self.0, message.as_bytes());
        crate::tickets::hex(&digest)[..MAC_HEX].to_owned()
    }
}

/// A prompt as a chat shows it, before its choices.
#[derive(Debug, Clone, Default)]
pub(crate) struct Card {
    /// The first line (`Approval needed: terminal`).
    pub(crate) title: String,
    /// One line each: the exact action, the working directory, the risk
    /// reasons, or the question.
    pub(crate) details: Vec<String>,
    /// Milliseconds until the request settles unanswered.
    pub(crate) deadline_ms: Option<u64>,
    /// What happens when nobody answers in time.
    pub(crate) on_expiry: String,
}

impl Card {
    /// The title, details and expiry line, as of `now`.
    pub(crate) fn text(&self, now: &Zoned) -> String {
        let mut text = visible(&self.title);
        for line in &self.details {
            let _ = write!(text, "\n{}", visible(line));
        }
        if let Some(ms) = self.deadline_ms {
            let _ = write!(text, "\n{}", expiry_line(ms, now, &self.on_expiry));
        }
        text
    }
}

/// `Expires at 14:05 UTC (in 5 min); <on_expiry>`: a chat message cannot
/// count down, so the wall-clock time is shown with the span it was sent
/// with.
pub(crate) fn expiry_line(deadline_ms: u64, now: &Zoned, on_expiry: &str) -> String {
    let span = SignedDuration::from_millis(i64::try_from(deadline_ms).unwrap_or(i64::MAX));
    let at = now.checked_add(span).map_or_else(
        |_| "later".to_owned(),
        |at| at.strftime("%H:%M %Z").to_string(),
    );
    let left = if deadline_ms < 60_000 {
        format!("{} s", deadline_ms.div_ceil(1000))
    } else {
        format!("{} min", deadline_ms.div_ceil(60_000))
    };
    let mut line = format!("Expires at {at} (in {left})");
    if !on_expiry.is_empty() {
        let _ = write!(line, "; {on_expiry}");
    }
    line
}

/// Model-controlled text shown in a chat: control characters are made
/// visible so they cannot disguise the command.
pub(crate) fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A button label for choice text, shortened to what platforms show.
pub(crate) fn label(text: &str) -> String {
    let text = visible(text);
    if text.chars().count() <= LABEL_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(LABEL_CHARS - 1).collect();
    short.push('\u{2026}');
    short
}

/// How a card's choices are offered in its text, per interactive tier.
pub(crate) fn instructions(tier: Interactive, id: &str) -> String {
    match tier {
        Interactive::Native => String::new(),
        Interactive::Numbered => format!(
            "\n1 Once / 2 Session / 3 Always / 4 Deny / 5 Explain\nReply with the number (or the word), optionally followed by {id}."
        ),
        Interactive::None => format!(
            "\nReply /approve {id} once, /approve {id} session, /approve {id} always, /approve {id} explain or /deny {id}."
        ),
    }
}

/// The choice a numbered reply stands for (`1`..`4` or the word,
/// optionally followed by the prompt id) and the id it quotes.
pub(crate) fn numbered_reply(text: &str) -> Option<(&'static str, Option<&str>)> {
    let mut words = text.split_whitespace();
    let first = words.next()?.to_lowercase();
    let quoted = words.next();
    if words.next().is_some() {
        return None;
    }
    let choice = match first.as_str() {
        "1" => "once",
        "2" => "session",
        "3" => "always",
        "4" => "deny",
        "5" | "explain" => "explain",
        word => CHOICES.into_iter().find(|c| *c == word)?,
    };
    Some((choice, quoted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_data_signed_with_this_boots_key_verifies() {
        let key = CardKey::random();
        let data = key.data(CardKind::Approval, "srq-42", "always");
        assert_eq!(
            key.verify(&data),
            Some((CardKind::Approval, "srq-42", "always"))
        );
        assert_eq!(key.verify("appr:srq-42:always:deadbeef"), None);
        let tampered = data.replace(":always:", ":once:");
        assert_eq!(key.verify(&tampered), None);
        assert_eq!(CardKey::random().verify(&data), None);
        // A signed clarify choice is no approval, and the other way round.
        let clarify = key.data(CardKind::Clarify, "srq-42", "0.1");
        assert_eq!(
            key.verify(&clarify),
            Some((CardKind::Clarify, "srq-42", "0.1"))
        );
        assert_eq!(key.verify(&clarify.replacen("clar:", "appr:", 1)), None);
        assert_eq!(key.verify(&data.replacen("appr:", "clar:", 1)), None);
    }

    #[test]
    fn callback_data_fits_telegrams_64_bytes() {
        let key = CardKey::random();
        let id = "srq-0123456789ab";
        assert!(key.data(CardKind::Approval, id, "session").len() <= 64);
        assert!(key.data(CardKind::Clarify, id, "9.4").len() <= 64);
    }

    #[test]
    fn the_expiry_shows_the_wall_clock_time_and_the_span() {
        let now: Zoned = "2026-11-02T14:00:00+00:00[UTC]".parse().unwrap();
        assert_eq!(
            expiry_line(300_000, &now, "unanswered, it is denied"),
            "Expires at 14:05 UTC (in 5 min); unanswered, it is denied"
        );
        assert_eq!(
            expiry_line(2_500, &now, ""),
            "Expires at 14:00 UTC (in 3 s)"
        );
    }

    #[test]
    fn numbered_replies_name_a_choice() {
        assert_eq!(numbered_reply("1"), Some(("once", None)));
        assert_eq!(numbered_reply("deny srq-9"), Some(("deny", Some("srq-9"))));
        assert_eq!(numbered_reply("5"), Some(("explain", None)));
        assert_eq!(numbered_reply("6"), None);
        assert_eq!(numbered_reply("1 is my answer"), None);
    }
}
