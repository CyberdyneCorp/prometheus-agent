//! OpenID Connect sign-in (`dashboard.auth.mode = "oidc"`): the
//! authorization code flow with PKCE against `dashboard.auth.oidc.issuer`,
//! for a public client (`client_id`, no client secret). The ID token's
//! signature (ES256 or RS256, from the issuer's JWKS), issuer, audience,
//! expiry and nonce are checked, and only `allowed_subjects` may sign in.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pmt_config::daemon_dashboard::OidcConfig;
use pmt_core::error::{Error, codes};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;

use super::secret::random_hex;

/// How long a sign-in may take between the redirect and the callback.
const PENDING_TTL: Duration = Duration::from_mins(10);
/// Sign-ins in progress kept at once; the oldest is dropped beyond it, so
/// anonymous `/login` requests cannot grow the daemon's memory.
const MAX_PENDING: usize = 1024;
/// Clock skew tolerated on `exp`.
const SKEW_SECS: i64 = 60;

#[derive(Debug)]
struct Pending {
    verifier: String,
    nonce: String,
    redirect_uri: String,
    at: Instant,
}

/// The issuer's endpoints.
#[derive(Debug, Clone)]
struct Discovery {
    authorization: String,
    token: String,
    jwks: String,
}

/// The OIDC sign-in of one listener.
#[derive(Debug)]
pub(crate) struct Oidc {
    issuer: String,
    client_id: String,
    allowed: Vec<String>,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, Pending>>,
}

fn failed(message: impl Into<String>) -> Error {
    Error::new(&codes::DASHBOARD_SIGNIN_FAILED, message)
}

impl Oidc {
    /// The sign-in for `config`; `None` when it is incomplete.
    pub(crate) fn new(config: &OidcConfig) -> Option<Self> {
        let http = pmt_net::http::client_builder(&pmt_net::http::OutboundPolicy::default())
            .ok()?
            .timeout(Duration::from_secs(15))
            .build()
            .ok()?;
        Some(Self {
            issuer: config.issuer.clone()?.trim_end_matches('/').to_owned(),
            client_id: config.client_id.clone()?,
            allowed: config.allowed_subjects.clone(),
            http,
            pending: Mutex::default(),
        })
    }

    async fn get_json(&self, url: &str) -> Result<Value, Error> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| failed(format!("the OIDC issuer is unreachable: {e}")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| failed(format!("the OIDC issuer answered badly: {e}")))?;
        serde_json::from_slice(&bytes).map_err(|_| failed("the OIDC issuer did not answer JSON"))
    }

    async fn discover(&self) -> Result<Discovery, Error> {
        let doc = self
            .get_json(&format!("{}/.well-known/openid-configuration", self.issuer))
            .await?;
        let field = |name: &str| {
            doc[name]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| failed(format!("the OIDC discovery document has no {name}")))
        };
        Ok(Discovery {
            authorization: field("authorization_endpoint")?,
            token: field("token_endpoint")?,
            jwks: field("jwks_uri")?,
        })
    }

    /// The issuer URL the browser is sent to; the callback comes back to
    /// `redirect_uri`.
    pub(crate) async fn begin(&self, redirect_uri: &str) -> Result<String, Error> {
        let discovery = self.discover().await?;
        let (state, nonce, verifier) = (random_hex(16), random_hex(16), random_hex(32));
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = url::Url::parse(&discovery.authorization)
            .map_err(|_| failed("the OIDC authorization endpoint is not a URL"))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid")
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        let mut pending = self.pending.lock().expect("oidc poisoned");
        pending.retain(|_, p| p.at.elapsed() < PENDING_TTL);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, p)| p.at)
                .map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(
            state,
            Pending {
                verifier,
                nonce,
                redirect_uri: redirect_uri.to_owned(),
                at: Instant::now(),
            },
        );
        Ok(url.into())
    }

    /// Completes the sign-in of `state` with `code`: the signed-in subject.
    pub(crate) async fn complete(&self, code: &str, state: &str) -> Result<String, Error> {
        let pending = self
            .pending
            .lock()
            .expect("oidc poisoned")
            .remove(state)
            .filter(|p| p.at.elapsed() < PENDING_TTL)
            .ok_or_else(|| failed("unknown or expired sign-in; start again"))?;
        let discovery = self.discover().await?;
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &pending.redirect_uri)
            .append_pair("client_id", &self.client_id)
            .append_pair("code_verifier", &pending.verifier)
            .finish();
        let response = self
            .http
            .post(&discovery.token)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|e| failed(format!("the OIDC token exchange failed: {e}")))?;
        let tokens: Value = serde_json::from_slice(&response.bytes().await.unwrap_or_default())
            .map_err(|_| failed("the OIDC token endpoint did not answer JSON"))?;
        let id_token = tokens["id_token"]
            .as_str()
            .ok_or_else(|| failed("the OIDC token response has no id_token"))?;
        let jwks = self.get_json(&discovery.jwks).await?;
        let claims = verify_jwt(id_token, &jwks)?;
        self.check_claims(&claims, &pending.nonce)
    }

    fn check_claims(&self, claims: &Value, nonce: &str) -> Result<String, Error> {
        if claims["iss"].as_str().map(|i| i.trim_end_matches('/')) != Some(self.issuer.as_str()) {
            return Err(failed("the ID token is from another issuer"));
        }
        let audience_ok = match &claims["aud"] {
            Value::String(aud) => aud == &self.client_id,
            Value::Array(list) => list.iter().any(|a| a.as_str() == Some(&self.client_id)),
            _ => false,
        };
        if !audience_ok {
            return Err(failed("the ID token is for another client"));
        }
        let now = jiff::Timestamp::now().as_second();
        if claims["exp"]
            .as_i64()
            .is_none_or(|exp| exp + SKEW_SECS < now)
        {
            return Err(failed("the ID token has expired"));
        }
        if claims["nonce"].as_str() != Some(nonce) {
            return Err(failed("the ID token's nonce does not match the sign-in"));
        }
        let subject = claims["sub"]
            .as_str()
            .ok_or_else(|| failed("the ID token has no subject"))?;
        if !self.allowed.iter().any(|s| s == subject) {
            return Err(Error::new(
                &codes::DASHBOARD_SIGNIN_FAILED,
                format!("subject '{subject}' is not in dashboard.auth.oidc.allowed_subjects"),
            ));
        }
        Ok(subject.to_owned())
    }
}

fn decode(part: &str) -> Result<Vec<u8>, Error> {
    URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| failed("the ID token is malformed"))
}

/// Checks the JWT's signature against `jwks`; its claims.
pub(crate) fn verify_jwt(token: &str, jwks: &Value) -> Result<Value, Error> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(failed("the ID token is malformed"));
    };
    let header: Value =
        serde_json::from_slice(&decode(head)?).map_err(|_| failed("the ID token is malformed"))?;
    let signature = decode(sig)?;
    let message = format!("{head}.{body}");
    let alg = header["alg"].as_str().unwrap_or_default();
    let kid = header["kid"].as_str();
    let keys = jwks["keys"].as_array().cloned().unwrap_or_default();
    let verified = keys
        .iter()
        .filter(|k| kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)))
        .any(|key| verify_with(alg, key, message.as_bytes(), &signature));
    if !verified {
        return Err(failed("the ID token's signature does not verify"));
    }
    serde_json::from_slice(&decode(body)?).map_err(|_| failed("the ID token is malformed"))
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

    use super::verify_jwt;

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
    fn only_a_token_signed_by_the_issuers_key_verifies() {
        let (issuer, other) = (key(), key());
        let claims = json!({"sub": "alice"});
        let token = sign(&issuer, &claims);
        assert_eq!(verify_jwt(&token, &jwks(&issuer)).unwrap(), claims);
        assert!(verify_jwt(&token, &jwks(&other)).is_err());
        let forged = sign(&other, &claims);
        assert!(verify_jwt(&forged, &jwks(&issuer)).is_err());
        assert!(verify_jwt("a.b", &jwks(&issuer)).is_err());
    }
}
