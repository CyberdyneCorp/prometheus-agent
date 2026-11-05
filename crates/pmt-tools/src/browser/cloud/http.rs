//! What the first-party providers share: the API endpoint (the vendor's,
//! or one the user set in the secrets store), the host check against the
//! provider's manifest, and JSON requests whose errors never carry a URL
//! (a DevTools URL or query may hold a token).

use std::sync::Arc;

use pmt_core::secret::SecretString;
use serde_json::Value;

use super::Manifest;

/// Reads the signed-in subscription's access token (`None`: not signed
/// in).
pub type TokenSource = Arc<dyn Fn() -> Option<SecretString> + Send + Sync>;

/// A provider API's base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Without a trailing slash.
    pub base_url: String,
    /// The user chose it (a secrets-store URL): its host is allowed
    /// besides the manifest's.
    pub custom: bool,
}

impl Endpoint {
    /// The vendor's `default`, or the URL stored as `secret` (custom).
    #[must_use]
    pub fn of(default: &str, secret: Option<SecretString>) -> Self {
        match secret.map(|s| s.expose().trim().to_owned()) {
            Some(url) if !url.is_empty() => Self {
                base_url: url.trim_end_matches('/').to_owned(),
                custom: true,
            },
            _ => Self {
                base_url: default.to_owned(),
                custom: false,
            },
        }
    }

    /// `base_url` + `path`.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Checks that `url` goes to a host `manifest` declares, or to this
    /// endpoint's host when the user chose it.
    ///
    /// # Errors
    ///
    /// The refusal naming the host.
    pub fn check(&self, manifest: &Manifest, url: &str) -> Result<(), String> {
        let host = host_of(url).ok_or_else(|| format!("{}: unusable URL", manifest.id))?;
        let custom = self.custom && host_of(&self.base_url).as_deref() == Some(host.as_str());
        if custom || manifest.hosts.iter().any(|pattern| matches(pattern, &host)) {
            return Ok(());
        }
        Err(format!(
            "{}: {host} is not a host this provider declares",
            manifest.id
        ))
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase())
}

/// `*.example.com` matches subdomains; anything else matches exactly.
fn matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|rest| rest.ends_with('.')),
        None => pattern == host,
    }
}

/// A sent request's status and JSON body (`Null` when empty or not JSON).
///
/// # Errors
///
/// A transport failure, named without its URL.
pub(super) async fn send(request: reqwest::RequestBuilder) -> Result<(u16, Value), String> {
    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            "request timed out".to_owned()
        } else if err.is_connect() {
            "connection failed".to_owned()
        } else {
            "request failed".to_owned()
        }
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// The error of a non-success reply: its status and the API's message.
pub(super) fn failure(provider: &str, status: u16, body: &Value) -> String {
    let message = ["message", "error", "detail"]
        .iter()
        .find_map(|key| match body.get(key) {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Object(inner)) => inner
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        });
    match message {
        Some(message) => format!("{provider}: HTTP {status}: {message}"),
        None => format!("{provider}: HTTP {status}"),
    }
}

/// The string at the first of `keys` in `body`.
pub(super) fn field(provider: &str, body: &Value, keys: &[&str]) -> Result<String, String> {
    keys.iter()
        .find_map(|key| body.get(key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{provider}: the reply has no {}", keys[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: Manifest = Manifest {
        id: "demo",
        description: "",
        secrets: &[],
        required: 0,
        hosts: &["api.demo.test", "*.cdp.demo.test"],
    };

    #[test]
    fn only_declared_hosts_and_the_users_endpoint_are_reached() {
        let vendor = Endpoint::of("https://api.demo.test", None);
        assert!(vendor.check(&MANIFEST, "https://api.demo.test/v1").is_ok());
        assert!(vendor.check(&MANIFEST, "wss://eu.cdp.demo.test/x").is_ok());
        assert!(
            vendor
                .check(&MANIFEST, "wss://cdp.demo.test.evil/x")
                .is_err()
        );
        assert!(vendor.check(&MANIFEST, "http://127.0.0.1:9/x").is_err());
        let custom = Endpoint::of(
            "https://api.demo.test",
            Some(SecretString::new("http://127.0.0.1:4000/")),
        );
        assert_eq!(custom.base_url, "http://127.0.0.1:4000");
        assert!(custom.check(&MANIFEST, "ws://127.0.0.1:9/devtools").is_ok());
        assert!(custom.check(&MANIFEST, "ws://10.0.0.1:9/devtools").is_err());
    }
}
