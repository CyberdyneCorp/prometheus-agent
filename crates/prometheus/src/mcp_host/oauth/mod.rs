//! OAuth for `auth = "oauth"` MCP servers (spec: plugin-system, MCP OAuth
//! for HTTP servers).
//!
//! `prometheus mcp login <server>` ([`login`]) signs in; afterwards every
//! start of the server takes its access token here ([`bearer`]), which
//! refreshes it shortly before it expires. Tokens live only in the
//! secrets store (`MCP_OAUTH_<SERVER>_ACCESS`, `_REFRESH`, and a
//! dynamically registered client's `_CLIENT_SECRET`); the rest of the
//! sign-in (issuer, endpoints, client id, expiry) is kept in
//! `<profile>/state/mcp-oauth/<server>.json`. A refresh runs under the
//! profile's cross-process refresh lock and re-reads the tokens after
//! taking it, so a single-use refresh token is never presented twice; it
//! is bound to the issuer that granted it and dropped when the server's
//! issuer changes. A refresh that fails for good parks the server: no
//! start (and so no browser, whatever process this is) until the person
//! runs `prometheus mcp login <server>`, which status reports.

mod callback;
pub mod discovery;
pub mod login;

use std::path::PathBuf;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::McpServerConfig;
use pmt_core::secret::SecretString;
use serde::{Deserialize, Serialize};

/// An access token is refreshed when it expires within this.
const EARLY: Duration = Duration::from_mins(1);

/// What the profile keeps of a server's sign-in, besides the tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The issuer that granted the tokens.
    pub issuer: String,
    /// Its token endpoint.
    pub token_endpoint: String,
    /// The client id used.
    pub client_id: String,
    /// The access token's expiry (Unix seconds), if the server said.
    pub expires_at: Option<i64>,
    /// The protected resource (the server URL).
    pub resource: String,
    /// Why the server is parked (a refresh failed for good).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<String>,
}

/// The secrets-store name of `server`'s `kind` (`ACCESS`, `REFRESH`,
/// `CLIENT_SECRET`).
#[must_use]
pub fn secret_name(server: &str, kind: &str) -> String {
    let key: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("MCP_OAUTH_{key}_{kind}")
}

fn record_path(scope: &ProfileScope, server: &str) -> PathBuf {
    scope
        .dir()
        .join("state")
        .join("mcp-oauth")
        .join(format!("{server}.json"))
}

/// The sign-in record of `server`, if it signed in.
#[must_use]
pub fn record(scope: &ProfileScope, server: &str) -> Option<Record> {
    let text = std::fs::read_to_string(record_path(scope, server)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the sign-in record of `server`.
///
/// # Errors
///
/// The file could not be written.
pub fn save_record(scope: &ProfileScope, server: &str, record: &Record) -> Result<(), String> {
    let path = record_path(scope, server);
    if let Some(dir) = path.parent() {
        pmt_core::private_fs::create_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    pmt_core::private_fs::write_atomic(&path, &bytes)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Forgets `server`'s sign-in: record and tokens.
///
/// # Errors
///
/// The secrets store failed.
pub fn forget(scope: &ProfileScope, server: &str) -> Result<(), String> {
    let _ = std::fs::remove_file(record_path(scope, server));
    for kind in ["ACCESS", "REFRESH", "CLIENT_SECRET"] {
        scope
            .secrets()
            .store()
            .remove(&secret_name(server, kind))
            .map_err(|e| e.message().to_owned())?;
    }
    Ok(())
}

/// What a background start tells about a server that needs the person.
#[must_use]
pub fn needs_login(server: &str) -> String {
    format!("needs re-authorization: prometheus mcp login {server}")
}

fn secret(scope: &ProfileScope, server: &str, kind: &str) -> Option<SecretString> {
    scope
        .secrets()
        .store()
        .get(&secret_name(server, kind))
        .ok()
        .flatten()
}

fn set_secret(scope: &ProfileScope, server: &str, kind: &str, value: &str) -> Result<(), String> {
    scope
        .secrets()
        .store()
        .set(
            &secret_name(server, kind),
            &SecretString::new(value.to_owned()),
        )
        .map_err(|e| e.message().to_owned())
}

/// The access token of server `name` for a start: refreshed when it is
/// about to expire. Never interactive.
///
/// # Errors
///
/// `needs re-authorization: prometheus mcp login <name>` when it never
/// signed in or is parked; a transient refresh failure.
pub(super) fn bearer(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Result<String, String> {
    let record = record(scope, name).ok_or_else(|| needs_login(name))?;
    if record.parked.is_some() {
        return Err(needs_login(name));
    }
    let access = secret(scope, name, "ACCESS").ok_or_else(|| needs_login(name))?;
    if !expiring(&record) {
        return Ok(access.expose().to_owned());
    }
    // Starts run on the MCP runtime (multi-threaded): wait for the
    // refresh without stalling its other connections.
    let (scope, name, server) = (scope.clone(), name.to_owned(), server.clone());
    let handle = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
    tokio::task::block_in_place(move || handle.block_on(refresh(&scope, &name, &server)))
}

fn expiring(record: &Record) -> bool {
    let now = jiff::Timestamp::now().as_second();
    let early = i64::try_from(EARLY.as_secs()).unwrap_or(60);
    record.expires_at.is_some_and(|at| at - early <= now)
}

/// Parks `name` for `why` (its tools become unavailable).
fn park(scope: &ProfileScope, name: &str, mut record: Record, why: &str) -> String {
    tracing::warn!(server = name, "MCP OAuth sign-in parked: {why}");
    record.parked = Some(why.to_owned());
    if let Err(err) = save_record(scope, name, &record) {
        tracing::error!(server = name, "cannot record the parked sign-in: {err}");
    }
    needs_login(name)
}

/// Refreshes `name`'s tokens under the profile's refresh lock.
async fn refresh(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Result<String, String> {
    let lock_dir = pmt_providers::auth::refresh::Refresher::lock_dir_of(scope.dir());
    let lock_path = lock_dir.join(format!("refresh-mcp-{name}"));
    let _lock = tokio::task::spawn_blocking(move || {
        pmt_core::private_fs::create_dir(&lock_dir)?;
        pmt_core::private_fs::lock_beside(&lock_path)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("cannot take the token refresh lock: {e}"))?;
    // Another process may have refreshed meanwhile.
    let record = record(scope, name).ok_or_else(|| needs_login(name))?;
    if record.parked.is_some() {
        return Err(needs_login(name));
    }
    if !expiring(&record)
        && let Some(access) = secret(scope, name, "ACCESS")
    {
        return Ok(access.expose().to_owned());
    }
    let Some(refresh_token) = secret(scope, name, "REFRESH") else {
        return Err(park(scope, name, record, "no refresh token"));
    };
    let client = login::client(scope)?;
    let (current, _) = discovery::discover(&client, &server.url).await?;
    if current.issuer.trim_end_matches('/') != record.issuer.trim_end_matches('/') {
        let _ = scope
            .secrets()
            .store()
            .remove(&secret_name(name, "REFRESH"));
        return Err(park(
            scope,
            name,
            record,
            "the server's authorization server changed",
        ));
    }
    let client_secret = login::client_secret(scope, name, server);
    let form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh_token.expose().to_owned()),
        ("client_id", record.client_id.clone()),
        ("resource", record.resource.clone()),
    ];
    match login::token_request(
        &client,
        &record.token_endpoint,
        form,
        client_secret.as_ref(),
    )
    .await
    {
        Ok(tokens) => {
            let access = tokens.access_token.clone();
            login::store_tokens(scope, name, record, &tokens)?;
            Ok(access)
        }
        Err(login::TokenError::Refused(why)) => {
            let _ = scope
                .secrets()
                .store()
                .remove(&secret_name(name, "REFRESH"));
            Err(park(
                scope,
                name,
                record,
                &format!("the refresh token was refused: {why}"),
            ))
        }
        Err(login::TokenError::Failed(why)) => Err(format!("token refresh failed: {why}")),
    }
}
