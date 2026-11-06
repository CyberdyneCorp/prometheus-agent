//! Minted short-lived credentials (spec: model-providers, Minted
//! short-lived credentials).
//!
//! A provider with `providers.<id>.key_cmd` gets its token from that
//! helper's stdout: one bare token, or JSON with `access_token` and
//! optional `expires_in`; more than one line is refused. A `token_mint`
//! provider without a helper (`vertex`) mints a Google access token from a
//! service-account JSON file whose path is the provider's secret
//! (`GOOGLE_APPLICATION_CREDENTIALS`), else from Application Default
//! Credentials (the user's gcloud file). Each token is cached until 60 s
//! before it expires (3000 s when no expiry is advertised); a request
//! refused with HTTP 401 drops the cached token so the retry mints anew.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jiff::{SignedDuration, Timestamp};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Value, json};

use crate::registry::Provider;
use crate::resolve::SecretSource;

/// Tokens are used until this long before they expire.
const EARLY: SignedDuration = SignedDuration::from_secs(60);
/// Lifetime assumed when none is advertised.
const DEFAULT_LIFETIME: SignedDuration = SignedDuration::from_secs(3000);
/// Longest a helper may run.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// Google's token endpoint.
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The secret holding the service-account file's path.
pub const GOOGLE_CREDENTIALS_SECRET: &str = "GOOGLE_APPLICATION_CREDENTIALS";
/// The scope Vertex AI tokens are minted for.
const CLOUD_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// One minted token and when it stops being used.
#[derive(Clone)]
struct Minted {
    token: SecretString,
    use_until: Timestamp,
}

/// Mints and caches a profile's short-lived credentials.
pub struct Minter {
    client: reqwest::Client,
    user_home: Option<PathBuf>,
    cache: Mutex<BTreeMap<String, Minted>>,
}

impl std::fmt::Debug for Minter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minter").finish_non_exhaustive()
    }
}

impl Minter {
    /// A minter posting to token endpoints with `client`; Application
    /// Default Credentials are looked up under `user_home`.
    #[must_use]
    pub fn new(client: reqwest::Client, user_home: Option<PathBuf>) -> Self {
        Self {
            client,
            user_home,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The current token of `provider`, minted when none is cached or the
    /// cached one is about to expire.
    ///
    /// # Errors
    ///
    /// `provider.missing_credential` naming what is missing or malformed.
    pub async fn token(
        &self,
        provider: &Provider,
        secrets: &dyn SecretSource,
    ) -> Result<SecretString, Error> {
        let now = Timestamp::now();
        if let Some(cached) = self.lock().get(provider.id()).filter(|m| m.use_until > now) {
            return Ok(cached.token.clone());
        }
        let (token, lifetime) = match &provider.settings().key_cmd {
            Some(command) => helper(provider.id(), command).await?,
            None => self.google(provider, secrets).await?,
        };
        let use_until = now + lifetime.unwrap_or(DEFAULT_LIFETIME) - EARLY;
        self.lock().insert(
            provider.id().to_owned(),
            Minted {
                token: token.clone(),
                use_until,
            },
        );
        Ok(token)
    }

    /// Drops `provider`'s cached token (the provider refused it).
    pub fn invalidate(&self, provider: &str) {
        self.lock().remove(provider);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Minted>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A Google access token from the service-account file the provider's
    /// secret names, else from Application Default Credentials.
    async fn google(
        &self,
        provider: &Provider,
        secrets: &dyn SecretSource,
    ) -> Result<(SecretString, Option<SignedDuration>), Error> {
        let id = provider.id();
        // The service-account key file's path (not a key):
        // `providers.<id>.key_secret`, else GOOGLE_APPLICATION_CREDENTIALS.
        let name = provider
            .settings()
            .key_secret
            .as_ref()
            .map_or(GOOGLE_CREDENTIALS_SECRET, |n| n.0.as_str());
        let path = secrets.secret(name)?.map(|v| PathBuf::from(v.expose()));
        let path = match path {
            Some(path) => path,
            None => self.adc_file().ok_or_else(|| no_google_credentials(id))?,
        };
        let text = std::fs::read_to_string(&path).map_err(|e| {
            missing(
                id,
                &format!("cannot read Google credentials {}: {e}", path.display()),
            )
        })?;
        let file: Value = serde_json::from_str(&text).map_err(|_| {
            missing(
                id,
                &format!("{} is not a Google credentials JSON file", path.display()),
            )
        })?;
        let fields = match file["type"].as_str() {
            Some("service_account") => service_account_grant(id, &file)?,
            Some("authorized_user") => authorized_user_grant(id, &file)?,
            other => {
                return Err(missing(
                    id,
                    &format!(
                        "{} has credentials type {} (expected service_account or authorized_user)",
                        path.display(),
                        other.unwrap_or("<none>")
                    ),
                ));
            }
        };
        let url = token_uri(id, &file)?;
        let pairs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let reply = super::token::post(&self.client, &url, &pairs, false).await?;
        if reply.status != 200 {
            return Err(missing(
                id,
                &format!("Google's token endpoint answered HTTP {}", reply.status),
            ));
        }
        token_of(id, &reply.body)
    }

    /// `<user home>/.config/gcloud/application_default_credentials.json`
    /// (`%APPDATA%\gcloud\...` on Windows), when it exists.
    fn adc_file(&self) -> Option<PathBuf> {
        let home = self.user_home.as_deref()?;
        let path = adc_path(home);
        path.is_file().then_some(path)
    }
}

fn adc_path(home: &Path) -> PathBuf {
    let base = if cfg!(windows) {
        home.join("AppData").join("Roaming")
    } else {
        home.join(".config")
    };
    base.join("gcloud")
        .join("application_default_credentials.json")
}

/// The file's `token_uri`: Google's, or a loopback endpoint.
fn token_uri(provider: &str, file: &Value) -> Result<String, Error> {
    let url = file["token_uri"].as_str().unwrap_or(GOOGLE_TOKEN_URL);
    let host = pmt_config::provider_urls::host_of(url).unwrap_or_default();
    let google = url.starts_with("https://") && host == "oauth2.googleapis.com";
    let loopback = url.starts_with("http://")
        && host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if google || loopback {
        Ok(url.to_owned())
    } else {
        Err(missing(
            provider,
            &format!("the credentials' token_uri {url} is not Google's token endpoint"),
        ))
    }
}

/// A JWT-bearer grant signed with the service account's key.
fn service_account_grant(
    provider: &str,
    file: &Value,
) -> Result<Vec<(&'static str, String)>, Error> {
    let (Some(email), Some(key)) = (file["client_email"].as_str(), file["private_key"].as_str())
    else {
        return Err(missing(
            provider,
            "the service-account file lacks client_email or private_key",
        ));
    };
    let now = Timestamp::now().as_second();
    let claims = json!({
        "iss": email,
        "scope": CLOUD_SCOPE,
        "aud": file["token_uri"].as_str().unwrap_or(GOOGLE_TOKEN_URL),
        "iat": now,
        "exp": now + 3600,
    });
    let assertion = sign_rs256(provider, key, &claims)?;
    Ok(vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_owned(),
        ),
        ("assertion", assertion),
    ])
}

/// A refresh-token grant of the user's gcloud sign-in.
fn authorized_user_grant(
    provider: &str,
    file: &Value,
) -> Result<Vec<(&'static str, String)>, Error> {
    let field = |name: &str| file[name].as_str().map(str::to_owned);
    let (Some(client_id), Some(secret), Some(refresh)) = (
        field("client_id"),
        field("client_secret"),
        field("refresh_token"),
    ) else {
        return Err(missing(
            provider,
            "the application default credentials lack client_id, client_secret or refresh_token",
        ));
    };
    Ok(vec![
        ("grant_type", "refresh_token".to_owned()),
        ("client_id", client_id),
        ("client_secret", secret),
        ("refresh_token", refresh),
    ])
}

/// `header.claims.signature` with RS256 over the PKCS#8 PEM `key`.
fn sign_rs256(provider: &str, pem: &str, claims: &Value) -> Result<String, Error> {
    let der: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = STANDARD
        .decode(der.trim())
        .map_err(|_| missing(provider, "the service account's private_key is not PEM"))?;
    let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|_| {
        missing(
            provider,
            "the service account's private_key is not an RSA key",
        )
    })?;
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "RS256", "typ": "JWT"}).to_string());
    let body = URL_SAFE_NO_PAD.encode(claims.to_string());
    let message = format!("{header}.{body}");
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| missing(provider, "signing the service-account assertion failed"))?;
    Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// Runs `command` through the platform shell and reads one token.
async fn helper(
    provider: &str,
    command: &str,
) -> Result<(SecretString, Option<SignedDuration>), Error> {
    let mut cmd = if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    };
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(HELPER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            missing(
                provider,
                "providers.<id>.key_cmd did not finish within 30 s",
            )
        })?
        .map_err(|e| missing(provider, &format!("providers.<id>.key_cmd cannot run: {e}")))?;
    if !output.status.success() {
        return Err(missing(
            provider,
            &format!("providers.<id>.key_cmd exited with {}", output.status),
        ));
    }
    parse_helper(provider, &String::from_utf8_lossy(&output.stdout))
}

/// One bare token, or JSON with `access_token` and optional `expires_in`.
fn parse_helper(
    provider: &str,
    stdout: &str,
) -> Result<(SecretString, Option<SignedDuration>), Error> {
    let text = stdout.trim();
    if text.is_empty() || text.lines().count() > 1 {
        return Err(missing(
            provider,
            "helper output must be one token: providers.<id>.key_cmd printed nothing or more than one line",
        ));
    }
    if text.starts_with('{') {
        let value: Value = serde_json::from_str(text)
            .map_err(|_| missing(provider, "helper output starts with '{' but is not JSON"))?;
        return token_of(provider, &value);
    }
    if text.split_whitespace().count() > 1 {
        return Err(missing(
            provider,
            "helper output must be one token: it holds whitespace",
        ));
    }
    Ok((SecretString::new(text), None))
}

/// `access_token` and `expires_in` of a token answer.
fn token_of(provider: &str, body: &Value) -> Result<(SecretString, Option<SignedDuration>), Error> {
    let token = body["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| missing(provider, "the minted answer holds no access_token"))?;
    let lifetime = body["expires_in"]
        .as_i64()
        .or_else(|| body["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .map(SignedDuration::from_secs);
    Ok((SecretString::new(token), lifetime))
}

fn missing(provider: &str, why: &str) -> Error {
    Error::new(
        &codes::PROVIDER_MISSING_CREDENTIAL,
        format!("cannot mint a credential for provider '{provider}': {why}"),
    )
}

fn no_google_credentials(provider: &str) -> Error {
    missing(
        provider,
        "no service-account file (secret GOOGLE_APPLICATION_CREDENTIALS) and no application default credentials",
    )
    .with_hint(
        "prometheus secrets set GOOGLE_APPLICATION_CREDENTIALS (the key file's path), or run: gcloud auth application-default login",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_is_one_token_or_json() {
        let (token, life) = parse_helper("p", "tok-123\n").unwrap();
        assert_eq!(token.expose(), "tok-123");
        assert_eq!(life, None);
        let (token, life) =
            parse_helper("p", r#"{"access_token": "ya29.x", "expires_in": 1200}"#).unwrap();
        assert_eq!(token.expose(), "ya29.x");
        assert_eq!(life, Some(SignedDuration::from_secs(1200)));
        let err = parse_helper("p", "one\ntwo\n").unwrap_err();
        assert_eq!(err.code().code, "provider.missing_credential");
        assert!(err.message().contains("helper output must be one token"));
        assert!(parse_helper("p", "").is_err());
        assert!(parse_helper("p", "two words").is_err());
    }

    #[test]
    fn only_googles_or_a_loopback_token_endpoint_is_used() {
        assert!(token_uri("v", &json!({})).is_ok());
        assert!(token_uri("v", &json!({"token_uri": "http://127.0.0.1:9/token"})).is_ok());
        assert!(token_uri("v", &json!({"token_uri": "https://evil.example/token"})).is_err());
    }
}
