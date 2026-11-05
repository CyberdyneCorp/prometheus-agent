//! Finding an MCP server's authorization server (spec: plugin-system, MCP
//! OAuth for HTTP servers).
//!
//! The protected resource metadata (RFC 9728) of the server's URL names
//! the authorization server (else the server's own origin is the issuer);
//! its metadata (RFC 8414, falling back to OpenID discovery) must name the
//! same issuer it was fetched for. Every endpoint is HTTPS, or plain HTTP
//! on a loopback address.

use reqwest::Url;
use serde::Deserialize;

/// What login and refresh need of the authorization server.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthServer {
    /// Its issuer identifier.
    pub issuer: String,
    /// The browser flow's endpoint.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// The token endpoint.
    pub token_endpoint: String,
    /// Dynamic client registration (RFC 7591).
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// The device flow's endpoint (RFC 8628).
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    /// It sends `iss` with the authorization response (RFC 9207).
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
    /// It accepts Client ID Metadata Documents.
    #[serde(default)]
    pub client_id_metadata_document_supported: bool,
    /// Scopes it advertises.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// Refuses an endpoint that is not HTTPS (or HTTP on loopback).
///
/// # Errors
///
/// The endpoint is not acceptable.
pub fn check_endpoint(url: &str) -> Result<Url, String> {
    let parsed = Url::parse(url).map_err(|e| format!("'{url}' is not a URL: {e}"))?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if loopback => Ok(parsed),
        _ => Err(format!("'{url}' must use https")),
    }
}

/// `https://host/.well-known/<name><path>` of `base`.
fn well_known(base: &Url, name: &str) -> String {
    let path = base.path().trim_end_matches('/');
    format!(
        "{}/.well-known/{name}{path}",
        base.origin().ascii_serialization()
    )
}

async fn get_json<T: for<'de> Deserialize<'de>>(client: &reqwest::Client, url: &str) -> Option<T> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The authorization server of the MCP server at `mcp_url`, and the scopes
/// its protected resource advertises.
///
/// # Errors
///
/// No metadata, an issuer mismatch, an insecure endpoint.
pub async fn discover(
    client: &reqwest::Client,
    mcp_url: &str,
) -> Result<(AuthServer, Vec<String>), String> {
    let resource = check_endpoint(mcp_url)?;
    let root = Url::parse(&resource.origin().ascii_serialization())
        .map_err(|e| format!("'{mcp_url}': {e}"))?;
    let mut prm: Option<ResourceMetadata> = None;
    for url in [
        well_known(&resource, "oauth-protected-resource"),
        well_known(&root, "oauth-protected-resource"),
    ] {
        prm = get_json(client, &url).await;
        if prm.is_some() {
            break;
        }
    }
    let (issuer, resource_scopes) = match prm {
        Some(meta) => (
            meta.authorization_servers
                .first()
                .cloned()
                .unwrap_or_else(|| resource.origin().ascii_serialization()),
            meta.scopes_supported,
        ),
        None => (resource.origin().ascii_serialization(), Vec::new()),
    };
    Ok((metadata(client, &issuer).await?, resource_scopes))
}

/// The metadata of `issuer`, checked to name it (RFC 8414 section 3.3).
///
/// # Errors
///
/// No metadata, an issuer mismatch, an insecure endpoint.
pub async fn metadata(client: &reqwest::Client, issuer: &str) -> Result<AuthServer, String> {
    let base = check_endpoint(issuer)?;
    let candidates = [
        well_known(&base, "oauth-authorization-server"),
        well_known(&base, "openid-configuration"),
        format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        ),
    ];
    for url in candidates {
        let Some(server) = get_json::<AuthServer>(client, &url).await else {
            continue;
        };
        if server.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(format!(
                "the authorization server metadata at {url} names issuer '{}', not '{issuer}'",
                server.issuer
            ));
        }
        check_endpoint(&server.token_endpoint)?;
        for endpoint in [
            &server.authorization_endpoint,
            &server.registration_endpoint,
            &server.device_authorization_endpoint,
        ]
        .into_iter()
        .flatten()
        {
            check_endpoint(endpoint)?;
        }
        return Ok(server);
    }
    Err(format!(
        "no OAuth authorization server metadata found for '{issuer}'"
    ))
}
