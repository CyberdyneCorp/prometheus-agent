//! RS256 JSON Web Tokens (RFC 7515, 7517, 7519): verifying the tokens a
//! platform signs its webhook requests with (Bot Framework, Google ID
//! tokens) against its published key set, and signing the assertions a
//! service account trades for an access token (Google OAuth 2.0 JWT
//! bearer grant).
//!
//! RSA is the textbook PKCS #1 v1.5 operation on `crypto-bigint`
//! residues, whose exponentiation runs in constant time over the
//! exponent: only RS256 is accepted, so a token naming `none`, an `HS*`
//! algorithm or anything else never verifies, whatever the key set holds.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crypto_bigint::modular::runtime_mod::{DynResidue, DynResidueParams};
use crypto_bigint::{U2048, U4096, Uint};
use serde_json::{Value, json};

/// Accepted clock skew on `exp` and `nbf`.
const SKEW_SECS: u64 = 300;

/// The DER `DigestInfo` prefix of a SHA-256 digest (RFC 8017 §9.2).
const SHA256_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// base64url without padding.
#[must_use]
pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn unb64url(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}

/// `base^exp mod modulus` of big-endian numbers, for moduli of up to
/// 4096 bits; `None` for an even or oversized modulus.
fn modpow(base: &[u8], exp: &[u8], modulus: &[u8]) -> Option<Vec<u8>> {
    let modulus = strip(modulus);
    if modulus.len() <= U2048::BYTES {
        modpow_with::<{ U2048::LIMBS }>(base, exp, modulus)
    } else if modulus.len() <= U4096::BYTES {
        modpow_with::<{ U4096::LIMBS }>(base, exp, modulus)
    } else {
        None
    }
}

fn strip(bytes: &[u8]) -> &[u8] {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    &bytes[zeros..]
}

fn uint<const L: usize>(bytes: &[u8]) -> Option<Uint<L>> {
    let bytes = strip(bytes);
    let width = Uint::<L>::BYTES;
    if bytes.len() > width {
        return None;
    }
    let mut padded = vec![0u8; width - bytes.len()];
    padded.extend_from_slice(bytes);
    Some(Uint::<L>::from_be_slice(&padded))
}

fn modpow_with<const L: usize>(base: &[u8], exp: &[u8], modulus: &[u8]) -> Option<Vec<u8>> {
    let n = uint::<L>(modulus)?;
    // `new` panics on an even modulus (never an RSA one): refuse it first.
    if modulus.last().is_none_or(|b| b & 1 == 0) {
        return None;
    }
    let params = DynResidueParams::new(&n);
    let x = uint::<L>(base)?;
    if x >= n {
        return None;
    }
    let result = DynResidue::new(&x, params).pow(&uint::<L>(exp)?).retrieve();
    // Big-endian bytes of the words, most significant word first.
    let bytes: Vec<u8> = result
        .as_words()
        .iter()
        .rev()
        .flat_map(|w| w.to_be_bytes())
        .collect();
    let len = modulus.len();
    Some(bytes[bytes.len() - len..].to_vec())
}

/// The PKCS #1 v1.5 encoding of a SHA-256 digest for a `len`-byte modulus.
fn encoded(digest: &[u8], len: usize) -> Option<Vec<u8>> {
    let pad = len.checked_sub(3 + SHA256_INFO.len() + digest.len())?;
    let mut em = vec![0x00, 0x01];
    em.extend(std::iter::repeat_n(0xff, pad));
    em.push(0x00);
    em.extend_from_slice(&SHA256_INFO);
    em.extend_from_slice(digest);
    Some(em)
}

/// An RSA public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

impl PublicKey {
    /// The key of a JWK (`kty` `RSA`, `n` and `e` base64url).
    #[must_use]
    pub fn from_jwk(jwk: &Value) -> Option<Self> {
        if jwk["kty"] != "RSA" {
            return None;
        }
        Some(Self {
            n: unb64url(jwk["n"].as_str()?)?,
            e: unb64url(jwk["e"].as_str()?)?,
        })
    }

    /// Whether `signature` is an RS256 signature of `message`.
    #[must_use]
    pub fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        let n = strip(&self.n);
        if signature.len() != n.len() {
            return false;
        }
        let Some(em) = encoded(&crate::sign::sha256(message), n.len()) else {
            return false;
        };
        modpow(signature, &self.e, n).is_some_and(|m| crate::sign::same(&m, &em))
    }
}

/// An RSA private key (PKCS #8 or PKCS #1 PEM).
pub struct PrivateKey {
    n: Vec<u8>,
    d: Vec<u8>,
}

impl PrivateKey {
    /// Reads a `PRIVATE KEY` or `RSA PRIVATE KEY` PEM block.
    ///
    /// # Errors
    ///
    /// Text that is not such a key.
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let body: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .map(str::trim)
            .collect();
        let der = crate::sign::base64_decode(&body).map_err(|e| format!("key PEM: {e}"))?;
        let rsa = if pem.contains("BEGIN RSA PRIVATE KEY") {
            der
        } else {
            pkcs8_inner(&der).ok_or("key PEM: not a PKCS #8 RSA key")?
        };
        // RSAPrivateKey: version, n, e, d, ...
        let mut fields = Der::new(&rsa).sequence().ok_or("key PEM: malformed")?;
        let _version = fields.integer();
        let n = fields.integer().ok_or("key PEM: no modulus")?;
        let _e = fields.integer();
        let d = fields.integer().ok_or("key PEM: no private exponent")?;
        Ok(Self {
            n: strip(n).to_vec(),
            d: strip(d).to_vec(),
        })
    }

    /// The RS256 signature of `message`.
    ///
    /// # Errors
    ///
    /// A key the arithmetic cannot use.
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let em = encoded(&crate::sign::sha256(message), self.n.len()).ok_or("key too short")?;
        modpow(&em, &self.d, &self.n).ok_or_else(|| "unusable RSA key".to_owned())
    }
}

/// The `RSAPrivateKey` inside a PKCS #8 `PrivateKeyInfo`.
fn pkcs8_inner(der: &[u8]) -> Option<Vec<u8>> {
    let mut info = Der::new(der).sequence()?;
    let _version = info.integer()?;
    let _algorithm = info.next(0x30)?;
    Some(info.next(0x04)?.to_vec())
}

/// A minimal DER reader over one level of TLVs.
struct Der<'a> {
    rest: &'a [u8],
}

impl<'a> Der<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// The contents of the next TLV, which must carry `tag`.
    fn next(&mut self, tag: u8) -> Option<&'a [u8]> {
        let (&found, rest) = self.rest.split_first()?;
        if found != tag {
            return None;
        }
        let (&first, mut rest) = rest.split_first()?;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count > 4 || rest.len() < count {
                return None;
            }
            let (digits, after) = rest.split_at(count);
            rest = after;
            digits
                .iter()
                .fold(0usize, |acc, b| (acc << 8) | usize::from(*b))
        };
        if rest.len() < len {
            return None;
        }
        let (contents, after) = rest.split_at(len);
        self.rest = after;
        Some(contents)
    }

    fn integer(&mut self) -> Option<&'a [u8]> {
        self.next(0x02)
    }

    /// A reader over the contents of the next SEQUENCE.
    fn sequence(&mut self) -> Option<Der<'a>> {
        self.next(0x30).map(Der::new)
    }
}

/// What a verified token's claims must say.
#[derive(Debug, Clone, Default)]
pub struct Expect<'a> {
    /// Accepted `iss` values (empty: any).
    pub issuers: &'a [&'a str],
    /// The `aud` the token must name.
    pub audience: &'a str,
    /// Now, in seconds since the epoch (`exp`/`nbf` unchecked when unknown).
    pub now: Option<u64>,
}

/// The claims of `token` once its RS256 signature verifies against one of
/// `jwks`'s keys (`{"keys": [...]}`, narrowed by the header's `kid`) and
/// its claims meet `expect`.
///
/// # Errors
///
/// Why it does not verify.
pub fn verify(token: &str, jwks: &Value, expect: &Expect<'_>) -> Result<Value, String> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed token".to_owned());
    };
    let json = |part: &str| -> Result<Value, String> {
        let bytes = unb64url(part).ok_or("malformed token")?;
        serde_json::from_slice(&bytes).map_err(|_| "malformed token".to_owned())
    };
    let header = json(head)?;
    let claims = json(body)?;
    if header["alg"] != "RS256" {
        return Err("token algorithm is not RS256".to_owned());
    }
    let signature = unb64url(sig).ok_or("malformed token")?;
    let kid = header["kid"].as_str();
    let message = format!("{head}.{body}");
    let signed = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|k| kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)))
        .filter_map(PublicKey::from_jwk)
        .any(|key| key.verifies(message.as_bytes(), &signature));
    if !signed {
        return Err("token signature does not verify".to_owned());
    }
    check(&claims, expect)?;
    Ok(claims)
}

fn check(claims: &Value, expect: &Expect<'_>) -> Result<(), String> {
    let issuer = claims["iss"].as_str().unwrap_or_default();
    if !expect.issuers.is_empty() && !expect.issuers.contains(&issuer) {
        return Err(format!("token issuer '{issuer}' is not accepted"));
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == expect.audience,
        Value::Array(list) => list.iter().any(|a| a == expect.audience),
        _ => false,
    };
    if !audience_ok {
        return Err("token audience does not match".to_owned());
    }
    if let Some(now) = expect.now {
        let exp = claims["exp"].as_u64().ok_or("token has no exp")?;
        if now > exp + SKEW_SECS {
            return Err("token expired".to_owned());
        }
        if claims["nbf"]
            .as_u64()
            .is_some_and(|nbf| nbf > now + SKEW_SECS)
        {
            return Err("token not yet valid".to_owned());
        }
    }
    Ok(())
}

/// `claims` as an RS256 JWT signed with `key` (header `kid` when given).
///
/// # Errors
///
/// The key cannot sign.
pub fn sign(key: &PrivateKey, kid: Option<&str>, claims: &Value) -> Result<String, String> {
    let mut header = json!({"alg": "RS256", "typ": "JWT"});
    if let Some(kid) = kid {
        header["kid"] = json!(kid);
    }
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let signature = key.sign(input.as_bytes())?;
    Ok(format!("{input}.{}", b64url(&signature)))
}

/// How long fetched keys are trusted before they are fetched again.
const KEYS_TTL_SECS: u64 = 24 * 3600;
/// The least time between two fetches forced by an unknown key.
const REFETCH_SECS: u64 = 300;

/// A platform's published signing keys, fetched on first use, again once a
/// day, and again (at most every five minutes) when a token names a key
/// the set lacks, as after a rotation.
#[derive(Debug, Clone)]
pub struct KeySet {
    /// An OpenID metadata document naming its `jwks_uri`, or the key set.
    url: String,
    openid: bool,
    keys: Value,
    /// Plugin clock ms of the last fetch.
    fetched: Option<u64>,
}

impl KeySet {
    /// Keys found through an OpenID metadata document's `jwks_uri`.
    #[must_use]
    pub fn openid(metadata_url: &str) -> Self {
        Self {
            url: metadata_url.to_owned(),
            openid: true,
            keys: Value::Null,
            fetched: None,
        }
    }

    /// Keys at a JWKS URL.
    #[must_use]
    pub fn jwks(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            openid: false,
            keys: Value::Null,
            fetched: None,
        }
    }

    fn fetch(&mut self) -> Result<(), String> {
        let url = if self.openid {
            let metadata = crate::net::get(&self.url, &[])?;
            metadata["jwks_uri"]
                .as_str()
                .ok_or("the OpenID metadata names no jwks_uri")?
                .to_owned()
        } else {
            self.url.clone()
        };
        self.keys = crate::net::get(&url, &[])?;
        self.fetched = Some(crate::clock::now_ms());
        Ok(())
    }

    fn older_than(&self, secs: u64) -> bool {
        self.fetched
            .is_none_or(|at| crate::clock::elapsed(at, secs))
    }

    /// [`verify`] against the set (fetched as needed), with now from
    /// [`crate::time::now`].
    ///
    /// # Errors
    ///
    /// The keys cannot be fetched or the token does not verify.
    pub fn verify(
        &mut self,
        token: &str,
        issuers: &[&str],
        audience: &str,
    ) -> Result<Value, String> {
        if self.older_than(KEYS_TTL_SECS) {
            self.fetch()?;
        }
        let expect = Expect {
            issuers,
            audience,
            now: crate::time::now(),
        };
        match verify(token, &self.keys, &expect) {
            Err(why) if why.contains("signature") && self.older_than(REFETCH_SECS) => {
                self.fetch()?;
                verify(token, &self.keys, &expect)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1024-bit test key (small, so debug-build tests stay fast).
    const PEM: &str = include_str!("../testdata/rsa1024.pem");

    fn public_of(pem: &str) -> Value {
        let key = PrivateKey::from_pem(pem).unwrap();
        json!({"keys": [{"kty": "RSA", "kid": "k1", "n": b64url(&key.n), "e": "AQAB"}]})
    }

    #[test]
    fn a_signed_token_verifies_and_claims_are_checked() {
        let key = PrivateKey::from_pem(PEM).unwrap();
        let jwks = public_of(PEM);
        let claims = json!({"iss": "https://issuer", "aud": "app", "exp": 2000, "nbf": 1000});
        let token = sign(&key, Some("k1"), &claims).unwrap();
        let expect = Expect {
            issuers: &["https://issuer"],
            audience: "app",
            now: Some(1500),
        };
        assert_eq!(verify(&token, &jwks, &expect).unwrap()["aud"], "app");
        let late = Expect {
            now: Some(9000),
            ..expect.clone()
        };
        assert!(
            verify(&token, &jwks, &late)
                .unwrap_err()
                .contains("expired")
        );
        let other = Expect {
            audience: "other",
            ..expect.clone()
        };
        assert!(verify(&token, &jwks, &other).is_err());
        // A changed payload, a stripped signature or alg none never verify.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = b64url(br#"{"iss":"https://issuer","aud":"app","exp":2000}"#);
        parts[1] = &forged;
        assert!(verify(&parts.join("."), &jwks, &expect).is_err());
        let none = format!("{}.{forged}.", b64url(br#"{"alg":"none"}"#));
        assert!(verify(&none, &jwks, &expect).is_err());
    }
}
