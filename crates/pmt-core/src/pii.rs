//! Optional PII redaction (spec: security-boundaries, Optional PII
//! redaction): with `privacy.redact_pii = true`, text bound for a model
//! provider, logs, trajectories or telemetry has phone numbers replaced by
//! `[REDACTED:phone]`, email addresses other than the profile owner's by
//! `[REDACTED:email]`, and messaging user ids by a stable keyed hash
//! `user_<10 hex>` (the same id always gets the same hash within a
//! profile, so the model can still tell people apart).
//!
//! User ids are recognized where Prometheus writes them (the `[name|id]`
//! attribution of group lines) and wherever an id registered for the
//! session (its platform user) appears. Delivery never reads redacted
//! text: replies are routed by the gateway's own records, so they still
//! reach the real recipient. This is independent of secret redaction.

use std::collections::BTreeSet;
use std::sync::{LazyLock, PoisonError, RwLock};

use regex::Regex;
use sha2::{Digest as _, Sha256};

/// What a phone number becomes.
pub const PHONE: &str = "[REDACTED:phone]";
/// What an email address becomes.
pub const EMAIL: &str = "[REDACTED:email]";

/// Phone numbers written with separators (`+1 415 555 0100`, `(555)
/// 123-4567`, `555-123-4567`) or in E.164 form (`+14155550100`). Bare digit
/// runs, dates, times, versions and IP addresses do not match.
static PHONE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:\+\d{1,3}[ .-]?)?(?:\(\d{2,4}\)[ .-]?|\b\d{2,4}[ .-])\d{3,4}[ .-]\d{3,4}\b",
        r"|\+\d{8,15}\b",
    ))
    .expect("valid phone pattern")
});

static EMAIL_ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}\b").expect("valid email pattern")
});

/// `[name|id]` at the start of a line: the gateway's attribution.
static ATTRIBUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\[([^|\]\n]*)\|([^\]\n|]+)\]").expect("valid attribution pattern")
});

/// `text` with every email address and delimited phone number replaced
/// (`[REDACTED:email]`, `[REDACTED:phone]`), without the profile owner's
/// exemption or user-id hashing: what an advisor privacy filter needs
/// (spec: mixture-of-agents, Privacy filter for advisor outputs).
#[must_use]
pub fn redact_contacts(text: &str) -> String {
    let out = EMAIL_ADDRESS.replace_all(text, EMAIL);
    PHONE_NUMBER.replace_all(&out, PHONE).into_owned()
}

/// One profile's PII redaction.
pub struct Pii {
    key: [u8; 32],
    owners: Vec<String>,
    ids: RwLock<BTreeSet<String>>,
}

impl std::fmt::Debug for Pii {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key: it would let anyone reverse the pseudonyms of ids
        // they can guess.
        f.debug_struct("Pii")
            .field("owners", &self.owners.len())
            .finish_non_exhaustive()
    }
}

impl Pii {
    /// Redaction keyed by `key` (the profile's), leaving `owner_emails`.
    #[must_use]
    pub fn new(key: [u8; 32], owner_emails: &[String]) -> Self {
        Self {
            key,
            owners: owner_emails
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect(),
            ids: RwLock::default(),
        }
    }

    /// Hashes `id` wherever it appears from now on.
    pub fn register_id(&self, id: &str) {
        let id = id.trim();
        if id.len() >= 3 {
            self.ids
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.to_owned());
        }
    }

    /// The stable pseudonym of user `id`: `user_` and 10 hex digits of
    /// HMAC-SHA256(profile key, id).
    #[must_use]
    pub fn user_hash(&self, id: &str) -> String {
        let mac = hmac_sha256(&self.key, id.as_bytes());
        mac.iter().take(5).fold("user_".to_owned(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    /// `text` with user ids, emails (other than the owner's) and phone
    /// numbers replaced.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let attributed = ATTRIBUTION.replace_all(text, |c: &regex::Captures<'_>| {
            format!("[{}|{}]", &c[1], self.user_hash(&c[2]))
        });
        let mut out = attributed.into_owned();
        for id in self
            .ids
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            if out.contains(id.as_str()) {
                let pattern = format!(r"\b{}\b", regex::escape(id));
                if let Ok(re) = Regex::new(&pattern) {
                    out = re
                        .replace_all(&out, self.user_hash(id).as_str())
                        .into_owned();
                }
            }
        }
        let out = EMAIL_ADDRESS.replace_all(&out, |c: &regex::Captures<'_>| {
            let address = c[0].to_ascii_lowercase();
            if self.owners.contains(&address) {
                c[0].to_owned()
            } else {
                EMAIL.to_owned()
            }
        });
        PHONE_NUMBER.replace_all(&out, PHONE).into_owned()
    }
}

/// HMAC-SHA256 (RFC 2104) with a 32-byte key.
fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::{EMAIL, PHONE, Pii, hmac_sha256};

    fn pii() -> Pii {
        Pii::new([7; 32], &["owner@example.com".into()])
    }

    #[test]
    fn phones_and_other_peoples_emails_are_replaced() {
        let p = pii();
        for (text, want) in [
            ("call +1 415 555 0100 now", format!("call {PHONE} now")),
            ("(555) 123-4567", PHONE.to_owned()),
            ("555-123-4567.", format!("{PHONE}.")),
            ("+14155550100", PHONE.to_owned()),
            ("mail alice@corp.example", format!("mail {EMAIL}")),
            ("me: Owner@Example.com", "me: Owner@Example.com".to_owned()),
        ] {
            assert_eq!(p.redact(text), want, "{text}");
        }
        for kept in [
            "2026-11-04 12:30:45",
            "build 123456789",
            "ip 192.168.10.1",
            "version 1.12.345",
            "commit a1b2c3d4e5f6",
        ] {
            assert_eq!(p.redact(kept), kept);
        }
    }

    #[test]
    fn user_ids_get_a_stable_keyed_hash() {
        let p = pii();
        p.register_id("123456789");
        let hash = p.user_hash("123456789");
        assert!(hash.starts_with("user_") && hash.len() == 15, "{hash}");
        assert_eq!(p.redact("from 123456789"), format!("from {hash}"));
        assert_eq!(
            p.redact("[Bob|987654321] hi\n[Ann|123456789] ok"),
            format!("[Bob|{}] hi\n[Ann|{hash}] ok", p.user_hash("987654321"))
        );
        assert_ne!(Pii::new([8; 32], &[]).user_hash("123456789"), hash);
    }

    #[test]
    fn hmac_matches_a_reference_implementation() {
        // Python: hmac.new(b"\x0b" * 32, b"Hi There", hashlib.sha256).
        let mac = hmac_sha256(&[0x0b; 32], b"Hi There");
        let hex = mac.iter().fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        });
        assert_eq!(
            hex,
            "198a607eb44bfbc69903a0f1cf2bbdc5ba0aa3f3d9ae3c1c7a3b1696a0b68cf7"
        );
    }
}
