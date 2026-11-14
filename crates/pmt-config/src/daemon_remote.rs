//! `daemon.toml [daemon.remote]` (spec: messaging-gateway, Remote client
//! attachment): the listener that lets desktop and TUI clients on other
//! machines attach to this daemon with paired device tokens. Off by
//! default; TLS is mandatory (a configured certificate, else a self-signed
//! one the daemon creates and whose fingerprint is printed for pinning).

use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::daemon_dashboard::DashboardTls;

/// The default remote attachment address.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:9121";

/// `[daemon.remote]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Bind the remote attachment listener.
    pub enabled: bool,
    /// Its address.
    pub listen: String,
    /// Its certificate (unset: a self-signed one under `<home>/run/`).
    pub tls: DashboardTls,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: DEFAULT_LISTEN.to_owned(),
            tls: DashboardTls::default(),
        }
    }
}

impl RemoteConfig {
    /// The parsed listen address.
    ///
    /// # Errors
    ///
    /// The validation message of a bad `listen` or half-set `tls`.
    pub fn validate(&self) -> Result<SocketAddr, String> {
        if self.tls.cert.is_some() != self.tls.key.is_some() {
            return Err("daemon.remote.tls: set both cert and key".to_owned());
        }
        self.listen.parse().map_err(|_| {
            format!(
                "daemon.remote.listen: '{}' is not an address such as {DEFAULT_LISTEN}",
                self.listen
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteConfig;

    #[test]
    fn the_remote_listener_is_off_by_default_on_port_9121() {
        let config = RemoteConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.validate().unwrap().port(), 9121);
        let bad = RemoteConfig {
            listen: "nowhere".into(),
            ..RemoteConfig::default()
        };
        assert!(bad.validate().unwrap_err().contains("daemon.remote.listen"));
    }
}
