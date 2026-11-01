//! Sign-in secrets: the PKCE verifier and its S256 challenge (RFC 7636),
//! the random `state`, and the constant-time comparison that checks a
//! callback's `state` (spec: model-providers, Device-code and browser PKCE
//! sign-in flows).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use sha2::{Digest as _, Sha256};

/// `n` random bytes from the OS, base64url without padding.
///
/// # Errors
///
/// `auth.login_failed` when the OS has no randomness to give.
pub fn random_token(bytes: usize) -> Result<SecretString, Error> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| {
        Error::new(
            &codes::AUTH_LOGIN_FAILED,
            format!("cannot read OS randomness for the sign-in: {e}"),
        )
    })?;
    Ok(SecretString::new(URL_SAFE_NO_PAD.encode(buf)))
}

/// A PKCE verifier (43 characters from 32 random bytes) and its challenge.
#[derive(Debug)]
pub struct Pkce {
    /// Sent only with the code exchange.
    pub verifier: SecretString,
    /// `BASE64URL(SHA256(verifier))`, sent with the authorization request.
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier and challenge.
    ///
    /// # Errors
    ///
    /// As [`random_token`].
    pub fn new() -> Result<Self, Error> {
        let verifier = random_token(32)?;
        let challenge = challenge(verifier.expose());
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// The S256 challenge of `verifier`.
#[must_use]
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Compares `a` and `b` in time independent of where they differ.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_the_rfc_7636_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_fresh_and_of_legal_length() {
        let a = Pkce::new().unwrap();
        let b = Pkce::new().unwrap();
        assert_eq!(a.verifier.expose().len(), 43);
        assert_ne!(a.verifier.expose(), b.verifier.expose());
        assert_eq!(a.challenge, challenge(a.verifier.expose()));
    }

    #[test]
    fn comparison_needs_equal_bytes() {
        assert!(constant_time_eq(b"state", b"state"));
        assert!(!constant_time_eq(b"state", b"statf"));
        assert!(!constant_time_eq(b"state", b"stat"));
    }
}
