//! Who an `http` MCP server is told the caller is (spec: plugin-system,
//! MCP mTLS and identity header).
//!
//! `client_cert` (a combined PEM, or the certificate with `client_key` for
//! the key) is presented in the TLS handshake, decrypting the key with
//! `client_key_password_secret`; a missing file fails the start with
//! `MCP server '<name>': client certificate not found: <path>`.
//! `identity_header = { name, value_from = "static" | "profile", value }`
//! sends the literal value or the active profile name on every request,
//! never overriding a same-named `headers` entry (compared
//! case-insensitively); an invalid block, or one on a `stdio` server, is
//! warned and ignored.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::schema::{McpServerConfig, McpTransport};
use pmt_net::ClientIdentity;
use serde_json::Value;

/// The client identity of server `name`, when it sets `client_cert`.
///
/// # Errors
///
/// A missing file, a bad certificate or key, a missing password secret.
pub(super) fn identity(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Result<Option<ClientIdentity>, String> {
    if server.client_cert.is_empty() {
        return Ok(None);
    }
    let password = if server.client_key_password_secret.is_empty() {
        None
    } else {
        Some(
            scope
                .secrets()
                .resolve(&server.client_key_password_secret)
                .map_err(|e| format!("MCP server '{name}': {}", e.message()))?,
        )
    };
    let key = (!server.client_key.is_empty()).then(|| Path::new(&server.client_key));
    ClientIdentity::load(
        Path::new(&server.client_cert),
        key,
        password
            .as_ref()
            .map(pmt_core::secret::SecretString::expose),
    )
    .map(Some)
    .map_err(|e| format!("MCP server '{name}': {}", e.message()))
}

/// The identity header of server `name` for `scope`'s profile, if it has
/// a valid one that `headers` does not already set.
pub(super) fn identity_header(
    scope: &ProfileScope,
    name: &str,
    server: &McpServerConfig,
) -> Option<(String, String)> {
    let block = server.identity_header.as_ref()?;
    if server.transport == McpTransport::Stdio {
        tracing::warn!(
            "mcp.servers.{name}.identity_header is ignored: it applies to http servers only"
        );
        return None;
    }
    let header = match parse(block, scope.profile().name().as_str()) {
        Ok(header) => header,
        Err(why) => {
            tracing::warn!("mcp.servers.{name}.identity_header is ignored: {why}");
            return None;
        }
    };
    let taken = server
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case(&header.0));
    (!taken).then_some(header)
}

/// The header of a block: `{ name, value_from, value? }`.
fn parse(block: &Value, profile: &str) -> Result<(String, String), String> {
    let table = block.as_object().ok_or("expected a table")?;
    if let Some(key) = table
        .keys()
        .find(|k| !matches!(k.as_str(), "name" | "value_from" | "value"))
    {
        return Err(format!("unknown key '{key}'"));
    }
    let text = |key: &str| table.get(key).and_then(Value::as_str);
    let header = text("name")
        .filter(|n| reqwest::header::HeaderName::from_bytes(n.as_bytes()).is_ok())
        .ok_or("name: expected a header name")?;
    let value = match text("value_from") {
        Some("profile") => profile.to_owned(),
        Some("static") => text("value")
            .filter(|v| !v.is_empty())
            .ok_or("value: a static identity header needs a value")?
            .to_owned(),
        _ => return Err("value_from: expected \"static\" or \"profile\"".to_owned()),
    };
    if reqwest::header::HeaderValue::from_str(&value).is_err() {
        return Err("value: not a valid header value".to_owned());
    }
    Ok((header.to_owned(), value))
}

#[cfg(test)]
mod tests {
    use super::parse;
    use serde_json::json;

    #[test]
    fn profile_and_static_values_and_invalid_blocks() {
        let profile = json!({"name": "X-User-Id", "value_from": "profile"});
        assert_eq!(
            parse(&profile, "alice").unwrap(),
            ("X-User-Id".to_owned(), "alice".to_owned())
        );
        let fixed = json!({"name": "X-Team", "value_from": "static", "value": "ops"});
        assert_eq!(parse(&fixed, "bob").unwrap().1, "ops");
        assert!(parse(&json!({"name": "X", "value_from": "static"}), "a").is_err());
        assert!(parse(&json!({"name": "X", "value_from": "env"}), "a").is_err());
        assert!(parse(&json!({"name": "bad name", "value_from": "profile"}), "a").is_err());
        assert!(parse(&json!("X-User-Id"), "a").is_err());
    }
}
