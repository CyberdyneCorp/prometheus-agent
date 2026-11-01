//! Sign-in endpoints per provider (spec: model-providers, Subscription and
//! OAuth sign-in registry; Device-code and browser PKCE sign-in flows).
//!
//! A built-in table holds the providers whose sign-in ships with this
//! build; `[providers.<id>.oauth]` replaces any field of it, or defines a
//! sign-in for a custom provider. Every endpoint is checked before the
//! first request: HTTPS (plain HTTP only on a loopback address) and a
//! host in the provider's allowed sign-in hosts.

use pmt_config::provider_urls::{host_of, is_local_host};
use pmt_config::schema::OAuthEndpoints;
use pmt_core::error::{Error, codes};

use crate::descriptor::host_matches;
use crate::registry::Provider;

/// How a device-code sign-in talks to the identity provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceProtocol {
    /// RFC 8628: device authorization, then polling the token endpoint.
    Rfc8628,
    /// OpenAI's: a user code, polling for an authorization code, then a
    /// PKCE code exchange at the token endpoint.
    OpenAi,
}

/// How the token endpoint wants its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenEncoding {
    /// `application/x-www-form-urlencoded` (RFC 6749).
    Form,
    /// JSON, with the `state` repeated in the code exchange (Anthropic).
    JsonWithState,
}

/// The loopback redirect of a browser sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// Host written in the redirect URI (`localhost` when the client is
    /// registered that way); the listener always binds `127.0.0.1`.
    pub uri_host: String,
    /// Fixed port, or 0 for an OS-assigned one.
    pub port: u16,
    /// Callback path.
    pub path: String,
}

/// One provider's sign-in endpoints and client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthSpec {
    /// Provider id.
    pub provider: String,
    /// OAuth client id.
    pub client_id: String,
    /// Browser flow authorization endpoint.
    pub authorize_url: Option<String>,
    /// Token endpoint.
    pub token_url: String,
    /// Device flow start endpoint.
    pub device_authorization_url: Option<String>,
    /// Device flow polling endpoint ([`DeviceProtocol::OpenAi`]).
    pub device_poll_url: Option<String>,
    /// Page where the user enters the code, when the start response does
    /// not name one.
    pub verification_url: Option<String>,
    /// Redirect URI of the device flow's code exchange
    /// ([`DeviceProtocol::OpenAi`]).
    pub device_redirect_uri: Option<String>,
    /// The device protocol.
    pub device_protocol: DeviceProtocol,
    /// Token revocation endpoint.
    pub revoke_url: Option<String>,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// The browser flow's redirect.
    pub redirect: Redirect,
    /// Extra authorization request parameters.
    pub authorize_params: Vec<(String, String)>,
    /// Token endpoint body encoding.
    pub token_encoding: TokenEncoding,
    /// Hosts the endpoints may be on.
    pub allowed_hosts: Vec<String>,
}

const OPENAI_ISSUER: &str = "https://auth.openai.com";

/// The sign-in of `provider`: the built-in endpoints with the profile's
/// `[providers.<id>.oauth]` fields over them.
///
/// # Errors
///
/// `auth.signin_unavailable` when neither exists or no token endpoint and
/// client id result.
pub fn for_provider(provider: &Provider) -> Result<OAuthSpec, Error> {
    let id = provider.id();
    let overrides = provider.settings().oauth.as_ref();
    let mut spec = builtin(id)
        .or_else(|| overrides.map(|_| custom(id)))
        .ok_or_else(|| unavailable(id))?;
    if let Some(o) = overrides {
        apply(&mut spec, o);
    }
    if spec.client_id.is_empty() || spec.token_url.is_empty() {
        return Err(unavailable(id).with_hint(format!(
            "set providers.{id}.oauth.client_id and providers.{id}.oauth.token_url"
        )));
    }
    Ok(spec)
}

fn unavailable(id: &str) -> Error {
    Error::new(
        &codes::AUTH_SIGNIN_UNAVAILABLE,
        format!("this build ships no sign-in endpoints for provider '{id}'"),
    )
    .with_hint(format!(
        "configure them under [providers.{id}.oauth], or use an API key: prometheus auth add {id} --type api_key"
    ))
}

fn custom(id: &str) -> OAuthSpec {
    OAuthSpec {
        provider: id.to_owned(),
        client_id: String::new(),
        authorize_url: None,
        token_url: String::new(),
        device_authorization_url: None,
        device_poll_url: None,
        verification_url: None,
        device_redirect_uri: None,
        device_protocol: DeviceProtocol::Rfc8628,
        revoke_url: None,
        scopes: Vec::new(),
        redirect: Redirect {
            uri_host: "127.0.0.1".to_owned(),
            port: 0,
            path: "/callback".to_owned(),
        },
        authorize_params: Vec::new(),
        token_encoding: TokenEncoding::Form,
        allowed_hosts: Vec::new(),
    }
}

/// The sign-ins this build ships: Claude subscriptions and ChatGPT plans.
fn builtin(id: &str) -> Option<OAuthSpec> {
    match id {
        "anthropic" => Some(OAuthSpec {
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_owned(),
            authorize_url: Some("https://claude.ai/oauth/authorize".to_owned()),
            token_url: "https://platform.claude.com/v1/oauth/token".to_owned(),
            scopes: owned(&["user:profile", "user:inference"]),
            redirect: Redirect {
                uri_host: "localhost".to_owned(),
                port: 0,
                path: "/callback".to_owned(),
            },
            authorize_params: vec![("code".to_owned(), "true".to_owned())],
            token_encoding: TokenEncoding::JsonWithState,
            allowed_hosts: owned(&["claude.ai", "platform.claude.com", "console.anthropic.com"]),
            ..custom(id)
        }),
        "openai-codex" => Some(OAuthSpec {
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
            authorize_url: Some(format!("{OPENAI_ISSUER}/oauth/authorize")),
            token_url: format!("{OPENAI_ISSUER}/oauth/token"),
            device_authorization_url: Some(format!(
                "{OPENAI_ISSUER}/api/accounts/deviceauth/usercode"
            )),
            device_poll_url: Some(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/token")),
            verification_url: Some(format!("{OPENAI_ISSUER}/codex/device")),
            device_redirect_uri: Some(format!("{OPENAI_ISSUER}/deviceauth/callback")),
            device_protocol: DeviceProtocol::OpenAi,
            scopes: owned(&["openid", "profile", "email", "offline_access"]),
            redirect: Redirect {
                uri_host: "localhost".to_owned(),
                port: CODEX_REDIRECT_PORT,
                path: "/auth/callback".to_owned(),
            },
            authorize_params: vec![
                ("id_token_add_organizations".to_owned(), "true".to_owned()),
                ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
            ],
            allowed_hosts: owned(&["auth.openai.com"]),
            ..custom(id)
        }),
        _ => None,
    }
}

/// The redirect port registered for the ChatGPT sign-in client.
pub const CODEX_REDIRECT_PORT: u16 = 1455;

fn apply(spec: &mut OAuthSpec, o: &OAuthEndpoints) {
    let set = |field: &mut Option<String>, value: &Option<String>| {
        if value.is_some() {
            field.clone_from(value);
        }
    };
    if let Some(client) = &o.client_id {
        spec.client_id.clone_from(client);
    }
    if let Some(token) = &o.token_url {
        spec.token_url.clone_from(token);
    }
    set(&mut spec.authorize_url, &o.authorize_url);
    set(
        &mut spec.device_authorization_url,
        &o.device_authorization_url,
    );
    set(&mut spec.device_poll_url, &o.device_poll_url);
    set(&mut spec.verification_url, &o.verification_url);
    set(&mut spec.revoke_url, &o.revoke_url);
    if let Some(scopes) = &o.scopes {
        spec.scopes.clone_from(scopes);
    }
    if let Some(port) = o.redirect_port {
        spec.redirect.port = port;
    }
    if let Some(path) = &o.redirect_path {
        spec.redirect.path.clone_from(path);
    }
    if let Some(hosts) = &o.allowed_hosts {
        spec.allowed_hosts.clone_from(hosts);
    }
}

impl OAuthSpec {
    /// Checks every endpoint before anything is sent: HTTPS, or plain HTTP
    /// to a loopback identity provider, and a host in `allowed_hosts`.
    ///
    /// # Errors
    ///
    /// `auth.insecure_endpoint`, `auth.host_not_allowed` naming the URL.
    pub fn check(&self) -> Result<(), Error> {
        let urls = [
            Some(&self.token_url),
            self.authorize_url.as_ref(),
            self.device_authorization_url.as_ref(),
            self.device_poll_url.as_ref(),
            self.revoke_url.as_ref(),
        ];
        for url in urls.into_iter().flatten() {
            self.check_url(url)?;
        }
        Ok(())
    }

    /// Checks only the token endpoint (a refresh contacts nothing else).
    ///
    /// # Errors
    ///
    /// As [`OAuthSpec::check`].
    pub fn check_token_endpoint(&self) -> Result<(), Error> {
        self.check_url(&self.token_url)
    }

    fn check_url(&self, url: &str) -> Result<(), Error> {
        let id = &self.provider;
        let host = host_of(url).unwrap_or_default();
        let secure = url.starts_with("https://")
            || (url.starts_with("http://") && is_loopback_ip_or_name(&host));
        if host.is_empty() || !secure {
            return Err(Error::new(
                &codes::AUTH_INSECURE_ENDPOINT,
                format!(
                    "sign-in endpoint '{url}' of provider '{id}' is not an https:// URL (plain http:// only on loopback)"
                ),
            ));
        }
        if !self.allowed_hosts.iter().any(|p| host_matches(p, &host)) {
            return Err(Error::new(
                &codes::AUTH_HOST_NOT_ALLOWED,
                format!(
                    "sign-in endpoint host '{host}' is not allowed for provider '{id}' (allowed: {})",
                    self.allowed_hosts.join(", ")
                ),
            )
            .with_hint(format!(
                "add it to providers.{id}.oauth.allowed_hosts if it is the provider's identity service"
            )));
        }
        Ok(())
    }
}

/// Loopback only (not private networks): a plain-HTTP identity provider
/// must run on this machine.
fn is_loopback_ip_or_name(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => is_local_host(bare) && (bare == "localhost" || bare.ends_with(".localhost")),
    }
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use pmt_config::Config;

    use super::*;
    use crate::ProviderRegistry;

    fn spec_of(config: &str, id: &str) -> Result<OAuthSpec, Error> {
        let config: Config = toml::from_str(config).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        for_provider(registry.lookup(id).unwrap())
    }

    #[test]
    fn chatgpt_sign_in_uses_the_registered_redirect() {
        let spec = spec_of("", "openai-codex").unwrap();
        assert_eq!(spec.redirect.port, 1455);
        assert_eq!(spec.redirect.path, "/auth/callback");
        assert_eq!(spec.redirect.uri_host, "localhost");
        spec.check().unwrap();
    }

    #[test]
    fn overrides_replace_fields_and_must_stay_on_allowed_hosts() {
        let moved =
            "[providers.openai-codex.oauth]\ntoken_url = \"http://127.0.0.1:9/oauth/token\"\n";
        let err = spec_of(moved, "openai-codex").unwrap().check().unwrap_err();
        assert_eq!(err.code().code, "auth.host_not_allowed");
        assert_eq!(err.exit_code(), 78);
        let allowed = format!("{moved}allowed_hosts = [\"127.0.0.1\", \"auth.openai.com\"]\n");
        spec_of(&allowed, "openai-codex").unwrap().check().unwrap();
    }

    #[test]
    fn plain_http_is_refused_off_loopback() {
        let config =
            "[providers.openai-codex.oauth]\ntoken_url = \"http://auth.openai.com/oauth/token\"\n";
        let err = spec_of(config, "openai-codex")
            .unwrap()
            .check()
            .unwrap_err();
        assert_eq!(err.code().code, "auth.insecure_endpoint");
    }

    #[test]
    fn providers_without_shipped_endpoints_say_so() {
        let err = spec_of("", "xai-oauth").unwrap_err();
        assert_eq!(err.code().code, "auth.signin_unavailable");
        assert_eq!(err.exit_code(), 69);
    }
}
