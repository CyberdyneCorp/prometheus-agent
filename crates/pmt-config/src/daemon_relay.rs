//! `daemon.toml [gateway.relay]` (spec: messaging-gateway, Relay
//! connector; Relay enrollment): the connector the daemon dials, its
//! gateway id and wake URL, and which fronted platforms may still connect
//! directly. Daemon-wide: one relay per home. The credential itself lives
//! only in the launch profile's secrets store, under `token_secret`.

use std::io;
use std::path::Path;

use pmt_core::error::{Error, codes};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::daemon::{DaemonConfig, FILE_NAME};
use crate::schema::SecretName;

/// The default secret holding the relay credential.
pub const DEFAULT_TOKEN_SECRET: &str = "PMT_RELAY_TOKEN";

/// `[gateway.relay]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    /// The connector's WebSocket URL (`wss://`; `ws://` only to a loopback
    /// host). Unset: no relay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The secret holding the gateway credential `prometheus gateway
    /// enroll` obtained (never the value).
    pub token_secret: SecretName,
    /// This gateway's id at the connector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_id: Option<String>,
    /// An `https://` URL the connector calls to wake a scaled-to-zero host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_url: Option<String>,
    /// Platforms the relay fronts whose direct adapters still connect.
    pub allow_direct_platforms: Vec<String>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            url: None,
            token_secret: SecretName(DEFAULT_TOKEN_SECRET.to_owned()),
            gateway_id: None,
            wake_url: None,
            allow_direct_platforms: Vec::new(),
        }
    }
}

/// Whether `url` may carry a credential: TLS, or plain only to a loopback
/// host (a local test connector).
#[must_use]
pub fn secure_url(url: &str, tls_scheme: &str, plain_scheme: &str) -> bool {
    if url
        .strip_prefix(tls_scheme)
        .is_some_and(|rest| rest.starts_with("://"))
    {
        return true;
    }
    let Some(rest) = url
        .strip_prefix(plain_scheme)
        .and_then(|r| r.strip_prefix("://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl RelayConfig {
    /// Checks the URLs.
    ///
    /// # Errors
    ///
    /// The message naming the bad key.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(url) = &self.url
            && !secure_url(url, "wss", "ws")
        {
            return Err(format!(
                "gateway.relay.url: '{url}' must be wss:// (ws:// only to a loopback host)"
            ));
        }
        if let Some(url) = &self.wake_url
            && !secure_url(url, "https", "http")
        {
            return Err(format!(
                "gateway.relay.wake_url: '{url}' must be https:// (http:// only to a loopback host)"
            ));
        }
        if !crate::schema::is_secret_name(&self.token_secret.0) {
            return Err(format!(
                "gateway.relay.token_secret: '{}' is not a secret name (A-Z, 0-9, _)",
                self.token_secret.0
            ));
        }
        Ok(())
    }
}

/// What enrollment writes to `daemon.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment<'a> {
    /// The connector URL.
    pub url: &'a str,
    /// The gateway id the connector assigned (or the one asked for).
    pub gateway_id: &'a str,
    /// The wake URL, when given.
    pub wake_url: Option<&'a str>,
}

/// Writes the relay settings of an enrollment into `<home>/daemon.toml`,
/// keeping the rest of the file (comments included) as it is.
///
/// # Errors
///
/// `config.parse` for an unreadable `daemon.toml` or an invalid result
/// (nothing is written); `home.io` when it cannot be written.
pub fn set_enrollment(home: &Path, enrollment: &Enrollment<'_>) -> Result<(), Error> {
    let path = home.join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::io(&codes::HOME_IO, &path, &e)),
    };
    let parse_error = |message: &str| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {message}", path.display()),
        )
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| parse_error(e.message()))?;
    let gateway = doc
        .entry("gateway")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| parse_error("gateway is not a table"))?;
    let relay = gateway
        .entry("relay")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| parse_error("gateway.relay is not a table"))?;
    relay["url"] = toml_edit::value(enrollment.url);
    relay["gateway_id"] = toml_edit::value(enrollment.gateway_id);
    match enrollment.wake_url {
        Some(wake) => relay["wake_url"] = toml_edit::value(wake),
        None => {
            relay.remove("wake_url");
        }
    }
    let updated = doc.to_string();
    let config: DaemonConfig = toml::from_str(&updated).map_err(|e| parse_error(e.message()))?;
    config
        .gateway
        .relay
        .validate()
        .map_err(|message| parse_error(&message))?;
    pmt_core::private_fs::write_atomic(&path, updated.as_bytes())
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tls_or_loopback_urls_carry_the_credential() {
        assert!(secure_url("wss://relay.example/gw", "wss", "ws"));
        assert!(secure_url("ws://127.0.0.1:9000/gw", "wss", "ws"));
        assert!(secure_url("ws://[::1]:9000", "wss", "ws"));
        assert!(!secure_url("ws://relay.example/gw", "wss", "ws"));
        assert!(!secure_url("https://relay.example", "wss", "ws"));
    }

    #[test]
    fn enrollment_keeps_the_rest_of_daemon_toml() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(FILE_NAME);
        std::fs::write(&file, "# mine\n[daemon]\nautostart = false\n").unwrap();
        set_enrollment(
            home.path(),
            &Enrollment {
                url: "wss://relay.example/gw",
                gateway_id: "gw-1",
                wake_url: Some("https://wake.example/x"),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# mine\n"));
        let config = DaemonConfig::load(home.path()).unwrap();
        assert!(!config.daemon.autostart);
        assert_eq!(
            config.gateway.relay.url.as_deref(),
            Some("wss://relay.example/gw")
        );
        assert_eq!(config.gateway.relay.gateway_id.as_deref(), Some("gw-1"));
        assert_eq!(config.gateway.relay.token_secret.0, DEFAULT_TOKEN_SECRET);
    }

    #[test]
    fn a_plain_remote_relay_url_is_refused() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(FILE_NAME),
            "[gateway.relay]\nurl = \"ws://relay.example\"\n",
        )
        .unwrap();
        let err = DaemonConfig::load(home.path()).unwrap_err();
        assert!(err.message().contains("gateway.relay.url"), "{err}");
    }
}
