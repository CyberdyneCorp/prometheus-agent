//! RS256 tokens for catalog adapter tests: a fixed 2048-bit test key
//! (`fixtures/catalog/rsa2048.pem`) signs what a platform would (Bot
//! Framework activities, Google ID tokens, service-account assertions
//! checked by a mock token endpoint), and its public half is the mock's
//! published key set. Also the `Date` header mocks send, since plugins
//! read the time from it.

#![allow(dead_code)]

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{
    RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_SHA256, RsaKeyPair, UnparsedPublicKey,
};
use serde_json::{Value, json};

/// The test key, PEM.
pub const PEM: &str = include_str!("../fixtures/catalog/rsa2048.pem");

fn der() -> Vec<u8> {
    let body: String = PEM.lines().filter(|l| !l.starts_with("-----")).collect();
    STANDARD.decode(body).unwrap()
}

fn key() -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(&der()).unwrap()
}

/// The key set publishing the test key as `kid`.
pub fn jwks(kid: &str) -> Value {
    let key = key();
    let parts = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(key.public());
    let (n, e) = (parts.n, parts.e);
    json!({"keys": [{"kty": "RSA", "use": "sig", "kid": kid, "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e)}]})
}

/// `claims` signed by the test key, header `kid`.
pub fn token(kid: &str, claims: &Value) -> String {
    let header = json!({"alg": "RS256", "typ": "JWT", "kid": kid});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let key = key();
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The claims of `token` when the test key signed it.
pub fn verified(token: &str) -> Option<Value> {
    let (input, signature) = token.rsplit_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let public = key().public().as_ref().to_vec();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
        .verify(input.as_bytes(), &signature)
        .ok()?;
    let claims = input.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    jiff::Timestamp::now().as_second().try_into().unwrap()
}

/// An HTTP `Date` header for now.
pub fn date_header() -> (String, String) {
    (
        "date".to_owned(),
        jiff::Timestamp::now()
            .strftime("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
    )
}
