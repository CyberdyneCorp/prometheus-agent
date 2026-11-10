//! Skill signatures (spec: skills, Skill signatures and provenance): a
//! detached `SKILL.sig` in the skill directory signing the canonical tree
//! digest of every other file, with a minisign key or a Sigstore keyless
//! identity ([`crate::keyless`]).
//!
//! ```json
//! {"version": 1, "digest": "sha256:...",
//!  "minisign": {"public_key": "RW...", "signature": "untrusted comment: ..."}}
//! ```
//!
//! The signature carries its key, so it proves who signed this exact
//! content (`verified:minisign:<key id>`), not that the signer is trusted:
//! trust stays the source's.

use std::path::Path;

use base64::Engine as _;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::keyless;

/// The signature file name.
pub const SIGNATURE_FILE: &str = "SKILL.sig";

/// A minisign signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Minisign {
    /// The base64 public key.
    pub public_key: String,
    /// The `.minisig` text over the digest.
    pub signature: String,
}

/// The `SKILL.sig` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureFile {
    /// Format version (1).
    pub version: u32,
    /// The signed digest.
    pub digest: String,
    /// A minisign signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minisign: Option<Minisign>,
    /// A Sigstore keyless signature bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sigstore: Option<keyless::Bundle>,
}

/// What a skill's signature says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// Valid, by this identity.
    Verified(String),
    /// No `SKILL.sig`.
    Unsigned,
    /// A `SKILL.sig` that does not verify, with why.
    Invalid(String),
}

impl Signature {
    /// The lockfile text: `verified:<identity>`, `unsigned` or `invalid`.
    #[must_use]
    pub fn record(&self) -> String {
        match self {
            Self::Verified(identity) => format!("verified:{identity}"),
            Self::Unsigned => "unsigned".to_owned(),
            Self::Invalid(_) => "invalid".to_owned(),
        }
    }
}

/// Checks the `SKILL.sig` content `sig` (if any) against `digest`, the
/// digest of every other file.
#[must_use]
pub fn verify(sig: Option<&[u8]>, digest: &str, roots: &keyless::TrustRoots) -> Signature {
    let Some(bytes) = sig else {
        return Signature::Unsigned;
    };
    let file: SignatureFile = match serde_json::from_slice(bytes) {
        Ok(file) => file,
        Err(e) => return Signature::Invalid(format!("unreadable {SIGNATURE_FILE}: {e}")),
    };
    if file.digest != digest {
        return Signature::Invalid("the signed digest does not match the content".to_owned());
    }
    if let Some(m) = &file.minisign {
        return match verify_minisign(digest, m) {
            Ok(id) => Signature::Verified(format!("minisign:{id}")),
            Err(why) => Signature::Invalid(why),
        };
    }
    if let Some(bundle) = &file.sigstore {
        return match keyless::verify(bundle, digest.as_bytes(), roots) {
            Ok(identity) => Signature::Verified(format!("sigstore:{identity}")),
            Err(why) => Signature::Invalid(why),
        };
    }
    Signature::Invalid(format!("{SIGNATURE_FILE} holds no signature"))
}

fn verify_minisign(digest: &str, m: &Minisign) -> Result<String, String> {
    let key = minisign_verify::PublicKey::from_base64(m.public_key.trim())
        .map_err(|e| format!("bad public key: {e}"))?;
    let signature = minisign_verify::Signature::decode(&m.signature)
        .map_err(|e| format!("bad signature: {e}"))?;
    key.verify(digest.as_bytes(), &signature, false)
        .map_err(|_| "the signature does not verify".to_owned())?;
    key_id(&m.public_key).ok_or_else(|| "bad public key".to_owned())
}

/// The key id of a base64 minisign public key (16 upper-case hex).
#[must_use]
pub fn key_id(public_key: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(public_key.trim())
        .ok()?;
    let bytes: [u8; 8] = raw.get(2..10)?.try_into().ok()?;
    Some(format!("{:016X}", u64::from_le_bytes(bytes)))
}

/// A minisign signature of `digest` with the unencrypted secret key file
/// at `key`.
///
/// # Errors
///
/// `skill.signature` when the key cannot be read or signing fails.
pub fn sign_minisign(digest: &str, key: &Path) -> Result<SignatureFile, Error> {
    let failed = |why: String| Error::new(&codes::SKILL_SIGNATURE, why);
    let text =
        std::fs::read_to_string(key).map_err(|e| failed(format!("{}: {e}", key.display())))?;
    let secret = minisign::SecretKeyBox::from_string(&text)
        .and_then(minisign::SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|e| {
            failed(format!(
                "{}: not an unencrypted minisign secret key: {e}",
                key.display()
            ))
        })?;
    let public = minisign::PublicKey::from_secret_key(&secret)
        .map_err(|e| failed(format!("cannot derive the public key: {e}")))?;
    let signature = minisign::sign(None, &secret, digest.as_bytes(), None, None)
        .map(minisign::SignatureBox::into_string)
        .map_err(|e| failed(format!("cannot sign: {e}")))?;
    Ok(SignatureFile {
        version: 1,
        digest: digest.to_owned(),
        minisign: Some(Minisign {
            public_key: public.to_base64(),
            signature,
        }),
        sigstore: None,
    })
}

/// The `SKILL.sig` text of `file`.
#[must_use]
pub fn render(file: &SignatureFile) -> String {
    let mut text = serde_json::to_string_pretty(file).unwrap_or_default();
    text.push('\n');
    text
}
