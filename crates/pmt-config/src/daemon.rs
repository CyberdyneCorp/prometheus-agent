//! `<home>/daemon.toml`: machine-wide settings shared by every profile of a
//! home (design D13). Only the sections M1 needs exist so far; the daemon
//! listeners, relay and update sections arrive with their capabilities.

use std::io;
use std::path::Path;

use pmt_core::error::{Error, codes};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// File name of the machine-wide settings in the home root.
pub const FILE_NAME: &str = "daemon.toml";

/// Root of `daemon.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Prometheus daemon.toml")]
pub struct DaemonConfig {
    /// Limits applied to every `prometheus` process of this home.
    #[serde(default)]
    pub process: ProcessConfig,
}

/// The `[process]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// Open-file soft limit every process raises itself to at startup on
    /// POSIX hosts, capped by the hard limit and never lowering a higher
    /// limit; children inherit it. `0` disables the raise.
    #[serde(default = "default_nofile_soft_limit")]
    pub nofile_soft_limit: u64,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            nofile_soft_limit: default_nofile_soft_limit(),
        }
    }
}

fn default_nofile_soft_limit() -> u64 {
    4096
}

impl DaemonConfig {
    /// Reads `<home>/daemon.toml`; a missing file means every default.
    ///
    /// # Errors
    ///
    /// `config.parse` (exit 78) for invalid TOML or an unknown key or
    /// value; `home.io` when the file exists but cannot be read.
    pub fn load(home: &Path) -> Result<Self, Error> {
        let path = home.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(Error::new(
                    &codes::HOME_IO,
                    format!("cannot read {}: {e}", path.display()),
                )
                .with_source(e));
            }
        };
        toml::from_str(&text).map_err(|e| {
            Error::new(
                &codes::CONFIG_PARSE,
                format!("{}: {}", path.display(), e.message()),
            )
            .with_hint(format!("fix or remove {}", path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DaemonConfig;

    fn load(text: &str) -> Result<DaemonConfig, pmt_core::Error> {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("daemon.toml"), text).unwrap();
        DaemonConfig::load(home.path())
    }

    #[test]
    fn missing_file_means_defaults() {
        let home = tempfile::tempdir().unwrap();
        let config = DaemonConfig::load(home.path()).unwrap();
        assert_eq!(config.process.nofile_soft_limit, 4096);
    }

    #[test]
    fn configured_limit_is_read() {
        let config = load("[process]\nnofile_soft_limit = 0\n").unwrap();
        assert_eq!(config.process.nofile_soft_limit, 0);
    }

    #[test]
    fn unknown_key_is_a_config_error() {
        let err = load("[process]\nnofile_limit = 10\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(err.code().code, "config.parse");
        assert!(err.message().contains("nofile_limit"), "{err}");
    }
}
