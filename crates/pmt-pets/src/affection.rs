//! Affection reactions (spec: pets, Affection reactions): a user message
//! with gratitude or affection for the agent, from a fixed local lexicon,
//! makes every surface showing the pet play the heart. Detection is a
//! plain match on the text the client already holds: no model call, and
//! the message is never changed.

/// Phrases matched as whole words, case-insensitively.
const PHRASES: &[&str] = &[
    "good bot",
    "good boy",
    "good girl",
    "good job",
    "nice work",
    "well done",
    "great job",
    "thank you",
    "thank u",
    "thanks",
    "thx",
    "ty",
    "love you",
    "love u",
    "ily",
    "you rock",
    "you're the best",
    "youre the best",
    "you are the best",
    "obrigado",
    "obrigada",
    "gracias",
    "merci",
    "danke",
];

/// Symbols matched anywhere.
const SYMBOLS: &[&str] = &[
    "<3",
    "\u{2764}",
    "\u{2665}",
    "\u{1f496}",
    "\u{1f495}",
    "\u{1f497}",
    "\u{1f49c}",
    "\u{1f499}",
    "\u{1f49a}",
    "\u{1f49b}",
    "\u{1f9e1}",
    "\u{1f970}",
    "\u{1f60d}",
    "\u{1f618}",
    "\u{1f64f}",
];

/// Whether `text` holds affection or gratitude aimed at the agent.
#[must_use]
pub fn is_affectionate(text: &str) -> bool {
    if SYMBOLS.iter().any(|s| text.contains(s)) {
        return true;
    }
    let words: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    PHRASES.iter().any(|phrase| {
        let wanted: Vec<&str> = phrase.split(' ').collect();
        words
            .windows(wanted.len())
            .any(|window| window.iter().zip(&wanted).all(|(w, p)| w == p))
    })
}

#[cfg(test)]
mod tests {
    use super::is_affectionate;

    /// Scenario: Thank-you message (detection half; the surfaces' tests
    /// show the heart and check the submitted text).
    #[test]
    fn thanks_and_good_bot_are_affection() {
        assert!(is_affectionate("thanks, good bot"));
        assert!(is_affectionate("Thank you!"));
        assert!(is_affectionate("this is great <3"));
        assert!(is_affectionate("\u{2764}\u{fe0f}"));
    }

    #[test]
    fn ordinary_requests_are_not() {
        for text in [
            "fix the build",
            "the bottom line",
            "thanksgiving plans",
            "type check it",
            "goodbye",
        ] {
            assert!(!is_affectionate(text), "{text}");
        }
    }
}
