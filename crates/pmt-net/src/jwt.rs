//! JSON Web Tokens verified against a JWKS (RFC 7515, 7517, 7519): the
//! signature must be by one of the set's keys under an asymmetric
//! algorithm, `ES256` or `RS256`. A symmetric (`HS*`) or unsigned
//! (`none`) token never verifies, whatever the set holds. Claims are the
//! caller's to check (the dashboard's OpenID Connect sign-in, the managed
//! scheduler's fire tokens).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// Why a token did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// Not three base64url parts with JSON header and claims.
    Malformed,
    /// No key of the set signed it under an allowed algorithm.
    Signature,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the token is malformed",
            Self::Signature => "the token's signature does not verify",
        })
    }
}

fn decode(part: &str) -> Result<Vec<u8>, Invalid> {
    URL_SAFE_NO_PAD.decode(part).map_err(|_| Invalid::Malformed)
}

/// The claims of `token` once its signature verifies against `jwks` (a
/// `{"keys": [...]}` set; a `kid` in the header narrows the keys tried).
///
/// # Errors
///
/// [`Invalid`].
pub fn verify(token: &str, jwks: &Value) -> Result<Value, Invalid> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Invalid::Malformed);
    };
    let header: Value = serde_json::from_slice(&decode(head)?).map_err(|_| Invalid::Malformed)?;
    let signature = decode(sig)?;
    let message = format!("{head}.{body}");
    let alg = header["alg"].as_str().unwrap_or_default();
    let kid = header["kid"].as_str();
    let verified = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|k| kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)))
        .any(|key| verify_with(alg, key, message.as_bytes(), &signature));
    if !verified {
        return Err(Invalid::Signature);
    }
    serde_json::from_slice(&decode(body)?).map_err(|_| Invalid::Malformed)
}

fn verify_with(alg: &str, key: &Value, message: &[u8], signature: &[u8]) -> bool {
    let field = |name: &str| {
        key[name]
            .as_str()
            .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
    };
    match (alg, key["kty"].as_str()) {
        ("ES256", Some("EC")) => {
            let (Some(x), Some(y)) = (field("x"), field("y")) else {
                return false;
            };
            let mut point = vec![0x04];
            point.extend(x);
            point.extend(y);
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                point,
            )
            .verify(message, signature)
            .is_ok()
        }
        ("RS256", Some("RSA")) => {
            let (Some(n), Some(e)) = (field("n"), field("e")) else {
                return false;
            };
            ring::signature::RsaPublicKeyComponents { n, e }
                .verify(
                    &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                    message,
                    signature,
                )
                .is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
    use serde_json::{Value, json};

    use super::{Invalid, verify};

    fn key() -> EcdsaKeyPair {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    fn jwks(key: &EcdsaKeyPair) -> Value {
        let point = key.public_key().as_ref();
        json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&point[33..65])}]})
    }

    fn sign(key: &EcdsaKeyPair, claims: &Value) -> String {
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"k1"}"#);
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        let message = format!("{head}.{body}");
        let sig = key.sign(&SystemRandom::new(), message.as_bytes()).unwrap();
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    #[test]
    fn only_a_token_signed_by_a_key_of_the_set_verifies() {
        let (issuer, other) = (key(), key());
        let claims = json!({"sub": "alice"});
        let token = sign(&issuer, &claims);
        assert_eq!(verify(&token, &jwks(&issuer)).unwrap(), claims);
        assert_eq!(verify(&token, &jwks(&other)), Err(Invalid::Signature));
        let forged = sign(&other, &claims);
        assert!(verify(&forged, &jwks(&issuer)).is_err());
        assert_eq!(verify("a.b", &jwks(&issuer)), Err(Invalid::Malformed));
    }

    #[test]
    fn symmetric_and_unsigned_tokens_never_verify() {
        let set =
            json!({"keys": [{"kty": "oct", "kid": "k1", "k": URL_SAFE_NO_PAD.encode(b"secret")}]});
        let body = URL_SAFE_NO_PAD.encode(br#"{"sub":"alice"}"#);
        for header in [r#"{"alg":"HS256","kid":"k1"}"#, r#"{"alg":"none"}"#] {
            let head = URL_SAFE_NO_PAD.encode(header);
            let message = format!("{head}.{body}");
            let mac = ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"secret"),
                message.as_bytes(),
            );
            let token = format!("{message}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()));
            assert_eq!(verify(&token, &set), Err(Invalid::Signature), "{header}");
        }
    }
}
