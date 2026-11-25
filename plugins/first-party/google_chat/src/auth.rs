//! Google credentials: the app's service-account token (a signed JWT
//! assertion traded at the token endpoint, RFC 7523; scopes `chat.bot`
//! and `pubsub`), the verification of the ID token Google signs HTTP
//! callbacks with (audience and service-account email as configured), and
//! each user's own refresh token from `/setup-files` (scope
//! `chat.messages.create`), kept as a JSON map by email in the secret
//! `GOOGLE_CHAT_USER_TOKENS` and traded for access tokens on demand.
//!
//! A bearer token only ever goes to a Google host or one of the API bases
//! the options configure ([`Urls::trusted`]).

use std::collections::HashMap;

use adapter_kit::jwt::{self, KeySet, PrivateKey};
use adapter_kit::oauth::Cached;
use adapter_kit::{host, net, time};
use serde_json::{Value, json};

pub(crate) const SERVICE_ACCOUNT: &str = "GOOGLE_CHAT_SERVICE_ACCOUNT_JSON";
pub(crate) const USER_TOKENS: &str = "GOOGLE_CHAT_USER_TOKENS";
pub(crate) const CLIENT_SECRET: &str = "GOOGLE_CHAT_OAUTH_CLIENT_SECRET";
const BOT_SCOPES: &str =
    "https://www.googleapis.com/auth/chat.bot https://www.googleapis.com/auth/pubsub";
/// What `/setup-files` asks each user for.
pub(crate) const USER_SCOPE: &str = "https://www.googleapis.com/auth/chat.messages.create";
const ID_TOKEN_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// The endpoints, overridable for tests and proxies.
#[derive(Debug, Clone)]
pub(crate) struct Urls {
    pub(crate) chat: String,
    pub(crate) pubsub: String,
    pub(crate) token: String,
    pub(crate) authorize: String,
    pub(crate) revoke: String,
}

fn base(options: &Value, key: &str, default: &str) -> String {
    options[key]
        .as_str()
        .unwrap_or(default)
        .trim_end_matches('/')
        .to_owned()
}

impl Urls {
    pub(crate) fn new(options: &Value) -> Self {
        Self {
            chat: base(options, "chat_api", "https://chat.googleapis.com"),
            pubsub: base(options, "pubsub_api", "https://pubsub.googleapis.com"),
            token: base(options, "token_url", "https://oauth2.googleapis.com/token"),
            authorize: base(
                options,
                "authorize_url",
                "https://accounts.google.com/o/oauth2/v2/auth",
            ),
            revoke: base(
                options,
                "revoke_url",
                "https://oauth2.googleapis.com/revoke",
            ),
        }
    }

    /// Whether a bearer token may go to `url`: a Google host, or under
    /// one of the configured API bases.
    pub(crate) fn trusted(&self, url: &str) -> bool {
        let configured = [&self.chat, &self.pubsub, &self.token]
            .iter()
            .any(|b| url.starts_with(&format!("{b}/")));
        let google = url.strip_prefix("https://").is_some_and(|rest| {
            let host = rest.split(['/', '?', ':']).next().unwrap_or_default();
            ["googleapis.com", "google.com", "googleusercontent.com"]
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        });
        configured || google
    }
}

/// The app's credentials.
pub(crate) struct Auth {
    pub(crate) urls: Urls,
    bot: Cached,
    /// Users' access tokens, by email.
    users: HashMap<String, Cached>,
    /// The ID-token check of HTTP callbacks: keys, audience, signer emails.
    callbacks: Option<(KeySet, String, Vec<String>)>,
    /// The users' refresh tokens as last written here (the secrets store
    /// takes the write at the next poll).
    tokens: Option<Value>,
}

impl Auth {
    pub(crate) fn new(options: &Value, http_mode: bool) -> Result<Self, String> {
        let callbacks = if http_mode {
            let audience = options["audience"]
                .as_str()
                .filter(|a| !a.is_empty())
                .ok_or("options.audience is required for HTTP callbacks")?
                .to_owned();
            let emails = adapter_kit::policy::strings(options, "service_account_email");
            let emails: Vec<String> = if emails.is_empty() {
                options["service_account_email"]
                    .as_str()
                    .map(|e| e.split(',').map(|s| s.trim().to_lowercase()).collect())
                    .unwrap_or_default()
            } else {
                emails.iter().map(|e| e.to_lowercase()).collect()
            };
            if emails.is_empty() {
                return Err(
                    "options.service_account_email is required for HTTP callbacks".to_owned(),
                );
            }
            let certs = base(
                options,
                "certs_url",
                "https://www.googleapis.com/oauth2/v3/certs",
            );
            Some((KeySet::jwks(&certs), audience, emails))
        } else {
            None
        };
        Ok(Self {
            urls: Urls::new(options),
            bot: Cached::default(),
            users: HashMap::new(),
            callbacks,
            tokens: None,
        })
    }

    /// Whether an HTTP callback's `Authorization` is a valid Google ID
    /// token for this app.
    pub(crate) fn verify_callback(&mut self, authorization: &str) -> Result<(), String> {
        let (keys, audience, emails) = self
            .callbacks
            .as_mut()
            .ok_or("HTTP callbacks are not configured")?;
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or("no bearer token")?;
        let claims = keys.verify(token, &ID_TOKEN_ISSUERS, audience)?;
        let email = claims["email"].as_str().unwrap_or_default().to_lowercase();
        if emails.contains(&email) {
            Ok(())
        } else {
            Err(format!("the token is signed for '{email}'"))
        }
    }

    /// `Bearer <token>` of the app's service account.
    pub(crate) fn bot(&mut self) -> Result<String, String> {
        if let Some(token) = self.bot.fresh() {
            return Ok(format!("Bearer {token}"));
        }
        let key: Value = host::secret(SERVICE_ACCOUNT)
            .and_then(|k| serde_json::from_str(&k).ok())
            .ok_or_else(|| {
                format!("permanent: secret {SERVICE_ACCOUNT} is not a service-account key")
            })?;
        let now = self.now()?;
        let claims = json!({
            "iss": key["client_email"],
            "scope": BOT_SCOPES,
            "aud": self.urls.token,
            "iat": now,
            "exp": now + 3600,
        });
        let private = PrivateKey::from_pem(key["private_key"].as_str().unwrap_or_default())?;
        let assertion = jwt::sign(&private, key["private_key_id"].as_str(), &claims)?;
        let token = self.bot.fetch(
            &self.urls.token,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ],
        )?;
        Ok(format!("Bearer {token}"))
    }

    /// Now, asking the token endpoint for its `Date` when nothing told it.
    fn now(&self) -> Result<u64, String> {
        if time::now().is_none() {
            let _ = net::send("GET", &self.urls.token, &[], None);
        }
        time::now().ok_or_else(|| "the time is unknown: no Date header seen".to_owned())
    }

    /// `Bearer <token>` of user `email`'s own grant, when they made one.
    pub(crate) fn user(&mut self, email: &str, client_id: &str) -> Result<Option<String>, String> {
        if let Some(token) = self.users.get(email).and_then(Cached::fresh) {
            return Ok(Some(format!("Bearer {token}")));
        }
        let Some(refresh) = self.tokens()[email].as_str().map(str::to_owned) else {
            return Ok(None);
        };
        let secret = host::secret(CLIENT_SECRET)
            .ok_or_else(|| format!("permanent: secret {CLIENT_SECRET} is not set"))?;
        let cached = self.users.entry(email.to_owned()).or_default();
        let token = cached.fetch(
            &self.urls.token,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", client_id),
                ("client_secret", &secret),
            ],
        )?;
        Ok(Some(format!("Bearer {token}")))
    }

    /// The users' refresh tokens, by email (an object).
    pub(crate) fn tokens(&self) -> Value {
        self.tokens.clone().unwrap_or_else(|| {
            host::secret(USER_TOKENS)
                .and_then(|t| serde_json::from_str(&t).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}))
        })
    }

    /// Sets (`Some`) or removes user `email`'s refresh token; the map to
    /// store in the secrets store.
    pub(crate) fn set_token(&mut self, email: &str, refresh: Option<&str>) -> Value {
        let mut tokens = self.tokens();
        match refresh {
            Some(token) => tokens[email] = json!(token),
            None => {
                if let Some(map) = tokens.as_object_mut() {
                    map.remove(email);
                }
            }
        }
        self.users.remove(email);
        self.tokens = Some(tokens.clone());
        tokens
    }
}
