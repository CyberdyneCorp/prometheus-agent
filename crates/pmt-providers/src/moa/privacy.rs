//! The privacy filter for advisor outputs (spec: mixture-of-agents,
//! Privacy filter for advisor outputs): `display` redacts known secrets,
//! credential-shaped tokens, email addresses and delimited phone numbers
//! in what users see and traces keep, while the aggregator reads the raw
//! text; `full` also redacts the aggregator's guidance. Bare digit runs,
//! line numbers, timestamps, git SHAs and IPv4 addresses are left alone.

use std::sync::LazyLock;

use pmt_config::schema::MoaPrivacyFilter;
use pmt_core::secret::Redactor;
use regex::Regex;

/// Dotted-quad IPv4 addresses, which the phone pattern could otherwise
/// cut into (`192.168.100.200`).
static IPV4: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").expect("valid IPv4 pattern"));

/// A preset call's filter.
#[derive(Debug, Clone)]
pub struct Filter<'a> {
    mode: MoaPrivacyFilter,
    secrets: &'a Redactor,
}

impl<'a> Filter<'a> {
    /// The filter of `mode`, with the profile's known `secrets`.
    #[must_use]
    pub fn new(mode: MoaPrivacyFilter, secrets: &'a Redactor) -> Self {
        Self { mode, secrets }
    }

    /// Advisor text as users see it.
    #[must_use]
    pub fn display(&self, text: &str) -> String {
        match self.mode {
            MoaPrivacyFilter::Off => text.to_owned(),
            MoaPrivacyFilter::Display | MoaPrivacyFilter::Full => redact(self.secrets, text),
        }
    }

    /// Text as a trace keeps it: the display filter, then known-secret
    /// redaction whatever the mode.
    #[must_use]
    pub fn trace(&self, text: &str) -> String {
        self.secrets.redact_all(&self.display(text))
    }

    /// Advisor text as the aggregator reads it.
    #[must_use]
    pub fn guidance(&self, text: &str) -> String {
        match self.mode {
            MoaPrivacyFilter::Full => redact(self.secrets, text),
            MoaPrivacyFilter::Off | MoaPrivacyFilter::Display => text.to_owned(),
        }
    }
}

/// A [`Filter`] that owns its secrets (for work done after the call
/// returns, such as a trace written when the stream ends).
#[derive(Debug, Clone)]
pub struct OwnedFilter {
    mode: MoaPrivacyFilter,
    secrets: Redactor,
}

impl OwnedFilter {
    /// The filter.
    #[must_use]
    pub fn filter(&self) -> Filter<'_> {
        Filter::new(self.mode, &self.secrets)
    }
}

impl Filter<'_> {
    /// This filter with its own copy of the secrets.
    #[must_use]
    pub fn to_owned_filter(&self) -> OwnedFilter {
        OwnedFilter {
            mode: self.mode,
            secrets: self.secrets.clone(),
        }
    }
}

/// `text` with secrets, emails and phone numbers redacted, IPv4
/// addresses kept whole.
fn redact(secrets: &Redactor, text: &str) -> String {
    let text = secrets.redact_all(text);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for ip in IPV4.find_iter(&text) {
        out.push_str(&pmt_core::pii::redact_contacts(&text[last..ip.start()]));
        out.push_str(ip.as_str());
        last = ip.end();
    }
    out.push_str(&pmt_core::pii::redact_contacts(&text[last..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_redacts_contacts_and_secrets_but_not_identifiers() {
        let secrets = Redactor::default();
        let filter = Filter::new(MoaPrivacyFilter::Display, &secrets);
        let shown = filter.display(
            "mail alice@example.com or call (555) 123-4567 / 555-123-4567; \
             commit a1b2c3d4e5f6 at 2026-11-08T10:45:30Z, line 1234, host 192.168.100.200, \
             key sk-abcdefghijklmnopqrstuvwxyz, run 123456789",
        );
        assert!(!shown.contains("alice@example.com"), "{shown}");
        assert!(!shown.contains("123-4567"), "{shown}");
        assert!(!shown.contains("sk-abcdefghijklmnop"), "{shown}");
        for kept in [
            "a1b2c3d4e5f6",
            "2026-11-08T10:45:30Z",
            "line 1234",
            "192.168.100.200",
            "123456789",
        ] {
            assert!(shown.contains(kept), "{kept} in {shown}");
        }
        assert!(
            filter
                .guidance("alice@example.com")
                .contains("alice@example.com")
        );
        let full = Filter::new(MoaPrivacyFilter::Full, &secrets);
        assert!(!full.guidance("alice@example.com").contains("alice@"));
        assert_eq!(full.guidance("commit a1b2c3d4e5f6"), "commit a1b2c3d4e5f6");
    }
}
