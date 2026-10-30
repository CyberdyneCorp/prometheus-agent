//! Secret values and the known-secret redactor.
//!
//! A [`SecretString`] never prints its value through `Debug`, so it cannot
//! leak through a stray `{:?}` in a log line or crash report. The
//! [`Redactor`] replaces every known secret value in free text (crash
//! reports now; trajectories and logs reuse it).

use std::fmt;

/// A secret value (API key, token). Only [`SecretString::expose`] reveals it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Call this only where the value is sent to its
    /// consumer (an HTTP header, a child's environment).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

/// Replaces known secret values in text with [`Redactor::MARKER`].
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a secret containing another is replaced whole.
    values: Vec<SecretString>,
}

impl Redactor {
    /// What a redacted value is replaced with.
    pub const MARKER: &'static str = "[REDACTED]";

    /// Values shorter than this are not redacted: replacing every occurrence
    /// of a 2-character string would destroy the report while protecting
    /// nothing a real credential looks like.
    pub const MIN_LEN: usize = 6;

    /// A redactor for the given secret values.
    #[must_use]
    pub fn new<'a>(secrets: impl IntoIterator<Item = &'a SecretString>) -> Self {
        let mut values: Vec<SecretString> = secrets
            .into_iter()
            .filter(|s| s.expose().len() >= Self::MIN_LEN)
            .cloned()
            .collect();
        values.sort_by_key(|s| std::cmp::Reverse(s.expose().len()));
        values.dedup();
        Self { values }
    }

    /// `text` with every known secret value replaced.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.values.iter().fold(text.to_owned(), |acc, secret| {
            acc.replace(secret.expose(), Self::MARKER)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Redactor, SecretString};

    #[test]
    fn debug_never_shows_the_value() {
        let secret = SecretString::new("sk-live-123456");
        assert!(!format!("{secret:?}").contains("sk-live"));
        assert_eq!(secret.expose(), "sk-live-123456");
    }

    #[test]
    fn known_values_are_replaced_longest_first() {
        let short = SecretString::new("abcdef");
        let long = SecretString::new("abcdef-123456");
        let redactor = Redactor::new([&short, &long]);
        assert_eq!(
            redactor.redact("key=abcdef-123456 other=abcdef"),
            "key=[REDACTED] other=[REDACTED]"
        );
    }

    #[test]
    fn very_short_values_are_left_alone() {
        let redactor = Redactor::new([&SecretString::new("ab")]);
        assert_eq!(redactor.redact("about"), "about");
    }
}
