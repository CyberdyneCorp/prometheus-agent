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

/// Prefixes of common credential formats (OpenAI/Anthropic/OpenRouter
/// `sk-`, Stripe, GitHub, Slack, AWS access keys, GitLab, Google API keys,
/// Groq, xAI, Perplexity, Replicate): the one list both config validation
/// and pattern redaction use.
pub const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "gsk_",
    "xai-",
    "pplx-",
    "r8_",
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

/// Replaces known secret values in text with a marker: [`Redactor::MARKER`]
/// for anonymous values, `[REDACTED:<NAME>]` for named ones
/// ([`Redactor::named`]).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// `(value, marker)`, longest value first, so a secret containing
    /// another is replaced whole.
    values: Vec<(SecretString, String)>,
}

impl Redactor {
    /// What an anonymous value or a credential-shaped token is replaced with.
    pub const MARKER: &'static str = "[REDACTED]";

    /// Values shorter than this are not redacted: replacing every occurrence
    /// of a 2-character string would destroy the report while protecting
    /// nothing a real credential looks like.
    pub const MIN_LEN: usize = 6;

    /// A redactor for the given anonymous secret values.
    #[must_use]
    pub fn new<'a>(secrets: impl IntoIterator<Item = &'a SecretString>) -> Self {
        Self::build(
            secrets
                .into_iter()
                .map(|s| (s.clone(), Self::MARKER.to_owned())),
        )
    }

    /// A redactor that replaces each value with `[REDACTED:<name>]`
    /// (spec: security-boundaries, Known-secret redaction everywhere).
    #[must_use]
    pub fn named<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a SecretString)>) -> Self {
        Self::build(
            secrets
                .into_iter()
                .map(|(name, s)| (s.clone(), format!("[REDACTED:{name}]"))),
        )
    }

    fn build(values: impl Iterator<Item = (SecretString, String)>) -> Self {
        let mut values: Vec<(SecretString, String)> = values
            .filter(|(s, _)| s.expose().len() >= Self::MIN_LEN)
            .flat_map(|(secret, marker)| {
                encoded_forms(secret.expose())
                    .into_iter()
                    .map(move |form| (SecretString::new(form), marker.clone()))
            })
            .collect();
        values.sort_by(|a, b| {
            b.0.expose()
                .len()
                .cmp(&a.0.expose().len())
                .then_with(|| a.1.cmp(&b.1))
        });
        values.dedup_by(|a, b| a.0 == b.0);
        Self { values }
    }

    /// This redactor plus every value of `other` (for example the launch
    /// environment's values plus a profile's store).
    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        Self::build(self.values.iter().chain(&other.values).cloned())
    }

    /// Whether no value is known (pattern redaction still applies).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `text` with every known secret value replaced.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.values
            .iter()
            .fold(text.to_owned(), |acc, (secret, marker)| {
                acc.replace(secret.expose(), marker)
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

/// The value plus the encodings it is commonly leaked in: percent-encoded
/// (URLs, form bodies) and base64 (standard and URL-safe, padded or not;
/// plus the encoding of its longest 3-byte-aligned prefix, which is how it
/// starts inside `echo $KEY | base64` or any longer encoded blob).
fn encoded_forms(value: &str) -> Vec<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let bytes = value.as_bytes();
    let aligned = &bytes[..bytes.len() / 3 * 3];
    let mut forms = vec![
        value.to_owned(),
        percent_encode(value, false),
        percent_encode(value, true),
        STANDARD.encode(bytes),
        STANDARD_NO_PAD.encode(bytes),
        URL_SAFE.encode(bytes),
        URL_SAFE_NO_PAD.encode(bytes),
        STANDARD.encode(aligned),
        URL_SAFE.encode(aligned),
    ];
    forms.retain(|form| form.len() >= Redactor::MIN_LEN);
    forms.sort();
    forms.dedup();
    forms
}

/// RFC 3986 percent-encoding of everything but unreserved characters.
fn percent_encode(value: &str, lower: bool) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ if lower => format!("%{b:02x}"),
            _ => format!("%{b:02X}"),
        })
        .collect()
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
    fn named_values_carry_their_name_and_merge_keeps_the_longest_first() {
        let token = SecretString::new("ghp_value_of_token");
        let named = Redactor::named([("GITHUB_TOKEN", &token)]);
        assert_eq!(
            named.redact("echo ghp_value_of_token"),
            "echo [REDACTED:GITHUB_TOKEN]"
        );
        let merged = Redactor::new([&SecretString::new("value_of")]).merged(&named);
        assert_eq!(
            merged.redact("ghp_value_of_token value_of"),
            "[REDACTED:GITHUB_TOKEN] [REDACTED]"
        );
    }

    /// Spec: "exact match, plus URL-encoded and base64 forms".
    #[test]
    fn url_encoded_and_base64_forms_are_redacted_with_the_name() {
        use base64::Engine as _;
        use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
        let value = SecretString::new("pa$$/word+key?=1");
        let redactor = Redactor::named([("DB_PASSWORD", &value)]);
        let marker = "[REDACTED:DB_PASSWORD]";
        let leaks = [
            "pa$$/word+key?=1".to_owned(),
            "pa%24%24%2Fword%2Bkey%3F%3D1".to_owned(),
            "pa%24%24%2fword%2bkey%3f%3d1".to_owned(),
            STANDARD.encode("pa$$/word+key?=1"),
            URL_SAFE_NO_PAD.encode("pa$$/word+key?=1"),
        ];
        for leak in leaks {
            let text = format!("url=https://x.test/?p={leak}&ok");
            assert_eq!(
                redactor.redact(&text),
                format!("url=https://x.test/?p={marker}&ok"),
                "{leak}"
            );
        }
        // `echo $KEY | base64` encodes a trailing newline: the aligned prefix
        // is still caught.
        let piped = STANDARD.encode("pa$$/word+key?=1\n");
        assert!(redactor.redact(&piped).starts_with(marker), "{piped}");
    }

    #[test]
    fn provider_token_patterns_are_redacted() {
        let redactor = Redactor::default();
        for token in [
            "gsk_abcdefghijklmnop1234",
            "xai-abcdefghijklmnop1234",
            "pplx-abcdefghijklmnop1234",
            "sk-ant-api03-abcdefghijklmnop",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert_eq!(redactor.redact_all(&format!("t={token}")), "t=[REDACTED]");
        }
    }

    #[test]
    fn very_short_values_are_left_alone() {
        let redactor = Redactor::new([&SecretString::new("ab")]);
        assert_eq!(redactor.redact("about"), "about");
    }
}
