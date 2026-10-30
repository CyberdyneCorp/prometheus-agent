//! Secret values and the known-secret redactor.
//!
//! A [`SecretString`] never prints its value through `Debug`, so it cannot
//! leak through a stray `{:?}` in a log line or crash report. The
//! [`Redactor`] replaces every known secret value in free text (crash
//! reports now; trajectories and logs reuse it), and with
//! [`Redactor::redact_all`] also anything shaped like a credential
//! ([`CREDENTIAL_PREFIXES`], PEM private keys).

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

/// Prefixes of common credential formats (OpenAI/Anthropic `sk-`, Stripe,
/// GitHub, Slack, AWS access keys, GitLab, Google API keys): the one list
/// both config validation and pattern redaction use.
pub const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "glpat-",
    "AIza",
];

/// Shortest credential-shaped token pattern redaction replaces: real keys
/// are 20+ characters, while shorter matches (`sk-learn`) are words.
const MIN_PATTERN_LEN: usize = 16;

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

    /// `text` with known values and every credential-shaped token (a
    /// [`CREDENTIAL_PREFIXES`] token of 16+ characters, a PEM private key
    /// block) replaced.
    #[must_use]
    pub fn redact_all(&self, text: &str) -> String {
        redact_patterns(&redact_pem(&self.redact(text)))
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

fn redact_patterns(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_token_char) {
        out.push_str(&rest[..start]);
        let token_len = rest[start..]
            .find(|c: char| !is_token_char(c))
            .unwrap_or(rest.len() - start);
        let token = &rest[start..start + token_len];
        let credential = token.len() >= MIN_PATTERN_LEN
            && CREDENTIAL_PREFIXES.iter().any(|p| token.starts_with(p));
        out.push_str(if credential { Redactor::MARKER } else { token });
        rest = &rest[start + token_len..];
    }
    out.push_str(rest);
    out
}

fn redact_pem(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const KEY_END: &str = "PRIVATE KEY-----";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let Some(header_end) = rest[start..]
            .find(KEY_END)
            .map(|i| start + i + KEY_END.len())
        else {
            break;
        };
        let Some(end) = rest[header_end..]
            .find(KEY_END)
            .map(|i| header_end + i + KEY_END.len())
        else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(Redactor::MARKER);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
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
    fn credential_shaped_tokens_and_pem_keys_are_redacted() {
        let redactor = Redactor::default();
        assert_eq!(
            redactor.redact_all("key: sk-live-abcdef1234567890, ok"),
            "key: [REDACTED], ok"
        );
        assert_eq!(
            redactor.redact_all("pip install sk-learn"),
            "pip install sk-learn"
        );
        let pem = "a -----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY----- b";
        assert_eq!(redactor.redact_all(pem), "a [REDACTED] b");
    }

    #[test]
    fn very_short_values_are_left_alone() {
        let redactor = Redactor::new([&SecretString::new("ab")]);
        assert_eq!(redactor.redact("about"), "about");
    }
}
