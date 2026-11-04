//! `prometheus mcp login <server>`: OAuth 2.1 with PKCE (spec:
//! plugin-system, MCP OAuth for HTTP servers).
//!
//! The client identifies with, in order: the configured `oauth.client_id`
//! (and `oauth.client_secret_name`), Prometheus' Client ID Metadata
//! Document when the authorization server accepts those, or a client
//! registered dynamically (RFC 7591). A server that offers none of these
//! (or refuses the registration) needs a pre-registered client: the login
//! exits 78 saying so. The browser flow prints the authorization URL
//! (opening a browser only from a foreground terminal), then takes the
//! redirect on the loopback listener or pasted by the person; the
//! response's `state` must match and its `iss`, when the server sends one
//! (or promises to), must name the issuer (RFC 9207). The device flow
//! (RFC 8628) prints a code to enter instead. Every request names the
//! server as the protected `resource` (RFC 8707).

use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::{McpOAuthFlow, McpServerConfig};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_net::OutboundPolicy;
use pmt_providers::auth::pkce::{Pkce, constant_time_eq, random_token};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};

use super::callback::{Callback, parse, pasted};
use super::discovery::{self, AuthServer};
use super::{Record, save_record, secret_name, set_secret};

/// Prometheus' Client ID Metadata Document (published with the sources).
pub const CLIENT_METADATA_DOCUMENT: &str = "https://raw.githubusercontent.com/CyberdyneCorp/prometheus-agent/main/docs/oauth/mcp-client-metadata.json";

/// The browser flow's callback path.
const CALLBACK_PATH: &str = "/callback";

/// Tokens from the token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    /// The access token.
    pub access_token: String,
    /// A refresh token, when one was granted (or rotated).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds until the access token expires.
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// Why a token request failed.
#[derive(Debug)]
pub enum TokenError {
    /// The server refused it for good (`invalid_grant`, `invalid_client`).
    Refused(String),
    /// A transient or unexpected failure.
    Failed(String),
}

/// How the login runs.
#[derive(Debug, Clone, Copy)]
pub struct LoginOptions {
    /// `browser` or `device`.
    pub flow: McpOAuthFlow,
    /// A browser may be opened (a foreground terminal, not over SSH).
    pub open_browser: bool,
}

/// The profile's outbound HTTP client.
///
/// # Errors
///
/// The client could not be built.
pub fn client(scope: &ProfileScope) -> Result<reqwest::Client, String> {
    let network = &scope.config().network;
    pmt_net::client(&OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    })
    .map_err(|e| e.message().to_owned())
}

/// The client secret of `name`: the configured one, else a dynamically
/// registered client's.
#[must_use]
pub fn client_secret(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Option<SecretString> {
    if !server.oauth.client_secret_name.is_empty() {
        return scope
            .secrets()
            .resolve(&server.oauth.client_secret_name)
            .ok();
    }
    scope
        .secrets()
        .store()
        .get(&secret_name(name, "CLIENT_SECRET"))
        .ok()
        .flatten()
}

/// One token endpoint request (form-encoded, client secret as HTTP basic
/// when there is one).
///
/// # Errors
///
/// [`TokenError`].
pub async fn token_request(
    client: &reqwest::Client,
    endpoint: &str,
    form: Vec<(&str, String)>,
    secret: Option<&SecretString>,
) -> Result<Tokens, TokenError> {
    let mut request = client.post(endpoint).header("Accept", "application/json");
    if let Some(secret) = secret {
        let id = form
            .iter()
            .find(|(k, _)| *k == "client_id")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        request = request.basic_auth(id, Some(secret.expose()));
    }
    let body = form_body(&form);
    let response = request
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| TokenError::Failed(e.to_string()))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_default();
    if status.is_success() {
        return serde_json::from_value(value).map_err(|e| TokenError::Failed(e.to_string()));
    }
    let error = value["error"]
        .as_str()
        .unwrap_or("unknown_error")
        .to_owned();
    if status.is_client_error()
        && matches!(
            error.as_str(),
            "invalid_grant" | "invalid_client" | "unauthorized_client"
        )
    {
        return Err(TokenError::Refused(error));
    }
    Err(TokenError::Failed(format!("HTTP {status}: {error}")))
}

/// `form` as an `application/x-www-form-urlencoded` body (empty values
/// left out).
fn form_body(form: &[(&str, String)]) -> String {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in form.iter().filter(|(_, v)| !v.is_empty()) {
        body.append_pair(key, value);
    }
    body.finish()
}

fn form_post(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, String)],
) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(form))
}

/// Stores `tokens` and the record of the sign-in.
///
/// # Errors
///
/// The secrets store or the record file failed.
pub fn store_tokens(
    scope: &ProfileScope,
    name: &str,
    mut record: Record,
    tokens: &Tokens,
) -> Result<(), String> {
    set_secret(scope, name, "ACCESS", &tokens.access_token)?;
    if let Some(refresh) = &tokens.refresh_token {
        set_secret(scope, name, "REFRESH", refresh)?;
    }
    record.expires_at = tokens
        .expires_in
        .map(|s| jiff::Timestamp::now().as_second() + s);
    record.parked = None;
    save_record(scope, name, &record)
}

fn failed(message: impl Into<String>) -> Error {
    Error::new(&codes::MCP_LOGIN_FAILED, message.into())
}

/// Signs `name` in and stores its tokens.
///
/// # Errors
///
/// `mcp.oauth_client_required` (exit 78) when the server needs a
/// pre-registered client; `mcp.login_failed` otherwise.
pub async fn login(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
    options: LoginOptions,
) -> Result<(), Error> {
    let client = client(scope).map_err(failed)?;
    let (auth, resource_scopes) = discovery::discover(&client, &server.url)
        .await
        .map_err(|why| failed(format!("MCP server '{name}': {why}")))?;
    let oauth = &server.oauth;
    let scopes = if oauth.scopes.is_empty() {
        resource_scopes
    } else {
        oauth.scopes.clone()
    };
    let timeout = Duration::from_secs(oauth.timeout_secs);
    let tokens = match options.flow {
        McpOAuthFlow::Browser => {
            let callback = Callback::bind(oauth.redirect_port, CALLBACK_PATH)
                .await
                .map_err(failed)?;
            let redirect_uri = if oauth.redirect_uri.is_empty() {
                format!(
                    "http://{}:{}{CALLBACK_PATH}",
                    oauth.redirect_host.as_str(),
                    callback.port()
                )
            } else {
                oauth.redirect_uri.clone()
            };
            let client_id = identify(scope, &client, &auth, name, server, &redirect_uri).await?;
            let browser = Browser {
                client: &client,
                auth: &auth,
                client_id: &client_id,
                scopes: &scopes,
                resource: &server.url,
                redirect_uri: &redirect_uri,
            };
            tokio::time::timeout(timeout, browser.run(&callback, options.open_browser))
                .await
                .map_err(|_| {
                    failed(format!(
                        "no login within {} s (oauth.timeout_secs)",
                        timeout.as_secs()
                    ))
                })??
                .with_client(client_id)
        }
        McpOAuthFlow::Device => {
            let client_id = identify(scope, &client, &auth, name, server, "").await?;
            tokio::time::timeout(
                timeout,
                device(&client, &auth, &client_id, &scopes, &server.url),
            )
            .await
            .map_err(|_| {
                failed(format!(
                    "no login within {} s (oauth.timeout_secs)",
                    timeout.as_secs()
                ))
            })??
            .with_client(client_id)
        }
    };
    let record = Record {
        issuer: auth.issuer.clone(),
        token_endpoint: auth.token_endpoint.clone(),
        client_id: tokens.1,
        expires_at: None,
        resource: server.url.clone(),
        parked: None,
    };
    store_tokens(scope, name, record, &tokens.0).map_err(failed)
}

trait WithClient {
    fn with_client(self, client_id: String) -> (Tokens, String);
}

impl WithClient for Tokens {
    fn with_client(self, client_id: String) -> (Tokens, String) {
        (self, client_id)
    }
}

/// The client id to use, registering one when the server allows it.
async fn identify(
    scope: &ProfileScope,
    client: &reqwest::Client,
    auth: &AuthServer,
    name: &str,
    server: &McpServerConfig,
    redirect_uri: &str,
) -> Result<String, Error> {
    if !server.oauth.client_id.is_empty() {
        return Ok(server.oauth.client_id.clone());
    }
    if auth.client_id_metadata_document_supported {
        return Ok(CLIENT_METADATA_DOCUMENT.to_owned());
    }
    let required = || {
        Error::new(
            &codes::MCP_OAUTH_CLIENT_REQUIRED,
            format!(
                "MCP server '{name}' needs a pre-registered OAuth client: its authorization server does not register clients dynamically"
            ),
        )
        .with_hint(format!(
            "register a client with the server's provider, then set mcp.servers.{name}.oauth.client_id (and oauth.client_secret_name for a confidential client)"
        ))
    };
    let Some(endpoint) = &auth.registration_endpoint else {
        return Err(required());
    };
    let client_name = if server.oauth.client_name.is_empty() {
        "Prometheus Agent".to_owned()
    } else {
        server.oauth.client_name.clone()
    };
    let mut body = json!({
        "client_name": client_name,
        "grant_types": ["authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if !redirect_uri.is_empty() {
        body["redirect_uris"] = json!([redirect_uri]);
    }
    let response = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| failed(format!("client registration failed: {e}")))?;
    if response.status().is_client_error() {
        return Err(required());
    }
    let registered: Value = response
        .json()
        .await
        .map_err(|e| failed(format!("client registration answered no JSON: {e}")))?;
    let client_id = registered["client_id"]
        .as_str()
        .ok_or_else(|| failed("client registration answered no client_id"))?
        .to_owned();
    if let Some(secret) = registered["client_secret"].as_str() {
        set_secret(scope, name, "CLIENT_SECRET", secret).map_err(failed)?;
    }
    Ok(client_id)
}

/// One browser login.
struct Browser<'a> {
    client: &'a reqwest::Client,
    auth: &'a AuthServer,
    client_id: &'a str,
    scopes: &'a [String],
    resource: &'a str,
    redirect_uri: &'a str,
}

impl Browser<'_> {
    async fn run(&self, callback: &Callback, open_browser: bool) -> Result<Tokens, Error> {
        let endpoint = self.auth.authorization_endpoint.as_deref().ok_or_else(|| {
            failed("the authorization server has no authorization endpoint (try --flow device)")
        })?;
        let pkce = Pkce::new()?;
        let state = random_token(32)?;
        let mut url = Url::parse(endpoint).map_err(|e| failed(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", self.client_id)
            .append_pair("redirect_uri", self.redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state.expose())
            .append_pair("resource", self.resource);
        if !self.scopes.is_empty() {
            url.query_pairs_mut()
                .append_pair("scope", &self.scopes.join(" "));
        }
        println!("Open this URL to sign in:\n  {url}");
        println!(
            "Waiting for the redirect; or paste the URL your browser was sent to and press Enter."
        );
        if open_browser {
            crate::mcp_host::oauth::login::open(url.as_str());
        }
        let query = tokio::select! {
            query = callback.wait(state.expose()) => query,
            pasted = pasted() => match pasted.and_then(|p| parse(&p, Url::parse(self.redirect_uri).map(|u| u.path().to_owned()).unwrap_or_default().as_str())) {
                Some(query) => query,
                None => return Err(failed("no redirect URL was pasted")),
            },
        };
        let code = self.check(&query, state.expose())?;
        let form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("redirect_uri", self.redirect_uri.to_owned()),
            ("client_id", self.client_id.to_owned()),
            ("code_verifier", pkce.verifier.expose().to_owned()),
            ("resource", self.resource.to_owned()),
        ];
        exchange(self.client, &self.auth.token_endpoint, form).await
    }

    /// The code of a genuine response.
    fn check(
        &self,
        query: &std::collections::HashMap<String, String>,
        state: &str,
    ) -> Result<String, Error> {
        if let Some(error) = query.get("error") {
            return Err(failed(format!(
                "the authorization server refused the login: {error}"
            )));
        }
        let given = query.get("state").map(String::as_bytes).unwrap_or_default();
        if !constant_time_eq(given, state.as_bytes()) {
            return Err(failed("the redirect's state does not match this login"));
        }
        match query.get("iss") {
            Some(iss) if iss.trim_end_matches('/') != self.auth.issuer.trim_end_matches('/') => {
                return Err(failed(format!(
                    "the redirect names issuer '{iss}', not '{}' (RFC 9207)",
                    self.auth.issuer
                )));
            }
            None if self.auth.authorization_response_iss_parameter_supported => {
                return Err(failed(
                    "the redirect lacks the issuer the server promised (RFC 9207)",
                ));
            }
            _ => {}
        }
        query
            .get("code")
            .cloned()
            .ok_or_else(|| failed("the redirect carries no authorization code"))
    }
}

async fn exchange(
    client: &reqwest::Client,
    endpoint: &str,
    form: Vec<(&str, String)>,
) -> Result<Tokens, Error> {
    token_request(client, endpoint, form, None)
        .await
        .map_err(|e| match e {
            TokenError::Refused(why) | TokenError::Failed(why) => {
                failed(format!("the token exchange failed: {why}"))
            }
        })
}

/// The device flow (RFC 8628).
async fn device(
    client: &reqwest::Client,
    auth: &AuthServer,
    client_id: &str,
    scopes: &[String],
    resource: &str,
) -> Result<Tokens, Error> {
    let endpoint = auth
        .device_authorization_endpoint
        .as_deref()
        .ok_or_else(|| {
            failed("the authorization server offers no device flow (use the browser flow)")
        })?;
    let mut form = vec![
        ("client_id", client_id.to_owned()),
        ("resource", resource.to_owned()),
    ];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }
    let started: Value = form_post(client, endpoint, &form)
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?
        .json()
        .await
        .map_err(|e| failed(e.to_string()))?;
    let field = |k: &str| started[k].as_str().map(str::to_owned);
    let device_code =
        field("device_code").ok_or_else(|| failed("the device flow answered no device_code"))?;
    let page = field("verification_uri_complete")
        .or_else(|| field("verification_uri"))
        .unwrap_or_default();
    println!(
        "Open {page} and enter the code {}",
        field("user_code").unwrap_or_default()
    );
    let mut interval = Duration::from_secs(started["interval"].as_u64().unwrap_or(5));
    loop {
        tokio::time::sleep(interval).await;
        let form = vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_owned(),
            ),
            ("device_code", device_code.clone()),
            ("client_id", client_id.to_owned()),
            ("resource", resource.to_owned()),
        ];
        let response = form_post(client, &auth.token_endpoint, &form)
            .send()
            .await
            .map_err(|e| failed(e.to_string()))?;
        let ok = response.status().is_success();
        let value: Value = response.json().await.unwrap_or_default();
        if ok {
            return serde_json::from_value(value).map_err(|e| failed(e.to_string()));
        }
        match value["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            other => {
                return Err(failed(format!(
                    "the device login failed: {}",
                    other.unwrap_or("unknown error")
                )));
            }
        }
    }
}

/// Opens `url` in the person's browser (a foreground terminal only).
pub fn open(url: &str) {
    let mut opener = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler");
        cmd
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = opener
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
