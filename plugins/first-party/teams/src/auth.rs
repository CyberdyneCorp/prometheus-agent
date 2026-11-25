//! Both directions of Bot Framework authentication: the JWT every inbound
//! request carries (RS256, issuer `https://api.botframework.com`,
//! audience the bot's app id, signed by a key of the Bot Framework's
//! published OpenID metadata, and naming the activity's `serviceUrl`), and
//! the client-credentials token the bot sends with (Entra ID, scope
//! `https://api.botframework.com/.default`), cached until shortly before
//! it expires.

use adapter_kit::jwt::KeySet;
use adapter_kit::oauth::ClientCredentials;
use serde_json::Value;

/// The issuer of Bot Connector tokens.
const ISSUER: &str = "https://api.botframework.com";
const SECRET: &str = "TEAMS_CLIENT_SECRET";
const SCOPE: &str = "https://api.botframework.com/.default";

/// What the adapter authenticates with.
#[derive(Debug, Clone)]
pub(crate) struct Auth {
    keys: KeySet,
    client_id: String,
    token: ClientCredentials,
}

impl Auth {
    pub(crate) fn new(options: &Value, client_id: &str, tenant: &str) -> Self {
        let metadata = options["openid_metadata_url"]
            .as_str()
            .unwrap_or("https://login.botframework.com/v1/.well-known/openidconfiguration");
        let login = options["login_base"]
            .as_str()
            .unwrap_or("https://login.microsoftonline.com")
            .trim_end_matches('/');
        Self {
            keys: KeySet::openid(metadata),
            client_id: client_id.to_owned(),
            token: ClientCredentials::new(
                &format!("{login}/{tenant}/oauth2/v2.0/token"),
                client_id,
                SECRET,
                SCOPE,
            ),
        }
    }

    /// The claims of a request's `Authorization: Bearer` token, checked
    /// before the body is even parsed.
    pub(crate) fn verify(&mut self, authorization: &str) -> Result<Value, String> {
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or("no bearer token")?;
        self.keys.verify(token, &[ISSUER], &self.client_id)
    }

    /// `Bearer <token>` for the Bot Connector.
    pub(crate) fn bearer(&mut self) -> Result<String, String> {
        self.token.bearer()
    }
}

/// Whether verified `claims` admit an activity from `service_url` (a token
/// naming another service URL does not).
pub(crate) fn names_service(claims: &Value, service_url: &str) -> bool {
    claims["serviceurl"]
        .as_str()
        .is_none_or(|claimed| claimed.trim_end_matches('/') == service_url.trim_end_matches('/'))
}
