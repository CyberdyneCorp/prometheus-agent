//! The release trust root (spec: distribution-and-update, Signature trust
//! root; security-boundaries, Signed releases): minisign public keys
//! compiled into the binary, a primary and a standby so the primary can be
//! rotated without stranding installs. Every release file is verified
//! against them before use, and nothing can switch the check off.
//!
//! Debug builds additionally trust `keys/test.pub`, whose secret key is
//! committed for tests (`tests/fixtures/test.key`). Release builds never
//! contain it, and a debug build is a source build (owner `dev`), which
//! refuses to self-update anyway.

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use pmt_core::error::{Error, codes};

/// One trusted key.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    role: &'static str,
    id: String,
    key: PublicKey,
}

impl TrustedKey {
    /// `primary`, `standby` (or `test` in debug builds).
    #[must_use]
    pub fn role(&self) -> &'static str {
        self.role
    }

    /// The minisign key id, as `minisign` prints it (16 upper-case hex).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// The keys every release file must be signed by (any one of them).
#[derive(Debug, Clone)]
pub struct TrustRoot {
    keys: Vec<TrustedKey>,
}

const PRIMARY: &str = include_str!("../keys/primary.pub");
const STANDBY: &str = include_str!("../keys/standby.pub");
#[cfg(debug_assertions)]
const TEST: &str = include_str!("../keys/test.pub");

impl TrustRoot {
    /// The keys compiled into this binary.
    ///
    /// # Panics
    ///
    /// Never for the committed key files (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut keys = vec![("primary", PRIMARY), ("standby", STANDBY)];
        #[cfg(debug_assertions)]
        keys.push(("test", TEST));
        Self::from_keys(&keys).expect("the embedded release keys parse")
    }

    /// A trust root of `(role, minisign .pub file text)` pairs.
    ///
    /// # Errors
    ///
    /// `update.signature` when a key does not parse.
    pub fn from_keys(keys: &[(&'static str, &str)]) -> Result<Self, Error> {
        let keys = keys
            .iter()
            .map(|(role, text)| parse_key(role, text))
            .collect::<Result<_, _>>()?;
        Ok(Self { keys })
    }

    /// The trusted keys, primary first.
    #[must_use]
    pub fn keys(&self) -> &[TrustedKey] {
        &self.keys
    }

    /// Verifies the minisign `signature` (the `.minisig` file text) of
    /// `data`, the contents of the release file `file`; returns the key
    /// that signed it.
    ///
    /// # Errors
    ///
    /// `update.signature` (exit 77) `signature verification failed for
    /// <file>` when the signature is malformed, made by an unknown key, or
    /// does not match the bytes.
    pub fn verify(&self, file: &str, data: &[u8], signature: &str) -> Result<&TrustedKey, Error> {
        let signature = Signature::decode(signature).map_err(|_| failed(file))?;
        self.keys
            .iter()
            .find(|trusted| trusted.key.verify(data, &signature, false).is_ok())
            .ok_or_else(|| failed(file))
    }
}

/// The error every failed check of a release file reports.
#[must_use]
pub fn failed(file: &str) -> Error {
    Error::new(
        &codes::UPDATE_SIGNATURE,
        format!("signature verification failed for {file}"),
    )
    .with_hint("the file is not what a Prometheus release key signed; nothing was installed")
}

fn parse_key(role: &'static str, text: &str) -> Result<TrustedKey, Error> {
    let invalid = || {
        Error::new(
            &codes::UPDATE_SIGNATURE,
            format!("the {role} release key is not a minisign public key"),
        )
    };
    let key = PublicKey::decode(text.trim()).map_err(|_| invalid())?;
    let b64 = text.trim().lines().last().ok_or_else(invalid)?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| invalid())?;
    let id_bytes: [u8; 8] = raw
        .get(2..10)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(invalid)?;
    Ok(TrustedKey {
        role,
        id: format!("{:016X}", u64::from_le_bytes(id_bytes)),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::TrustRoot;

    #[test]
    fn embedded_keys_parse_with_a_primary_and_a_standby() {
        let root = TrustRoot::embedded();
        let roles: Vec<_> = root.keys().iter().map(super::TrustedKey::role).collect();
        assert_eq!(&roles[..2], ["primary", "standby"]);
        assert_ne!(root.keys()[0].id(), root.keys()[1].id());
        assert!(root.keys().iter().all(|k| k.id().len() == 16));
    }

    #[test]
    fn garbage_signature_fails_with_the_file_name() {
        let err = TrustRoot::embedded()
            .verify("release-manifest.json", b"{}", "not a signature")
            .unwrap_err();
        assert_eq!(
            err.message(),
            "signature verification failed for release-manifest.json"
        );
        assert_eq!(err.exit_code(), 77);
    }
}
