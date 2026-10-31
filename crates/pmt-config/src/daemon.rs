//! `<home>/daemon.toml`: machine-wide settings shared by every profile of a
//! home (design D13). Only the sections M1 needs exist so far; the daemon
//! listeners, relay and update sections arrive with their capabilities.

use std::io;
use std::path::Path;
use std::sync::LazyLock;

use pmt_core::error::{Error, codes};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// File name of the machine-wide settings in the home root.
pub const FILE_NAME: &str = "daemon.toml";

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::to_value(schemars::schema_for!(DaemonConfig)).expect("the schema serializes")
});

/// The `daemon.toml` JSON Schema.
#[must_use]
pub fn schema() -> &'static serde_json::Value {
    &SCHEMA
}

/// Root of `daemon.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Prometheus daemon.toml", extend("x-apply" = "restart"))]
pub struct DaemonConfig {
    /// Limits applied to every `prometheus` process of this home.
    #[serde(default)]
    pub process: ProcessConfig,
    /// Self-update of this install (`prometheus update`).
    #[serde(default)]
    pub update: UpdateConfig,
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

/// The `[update]` section (spec: distribution-and-update). The release
/// channel is not here: it belongs to the install receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Seconds between background checks for a newer release; `0` disables
    /// them. A check sends only the version and target (in the user agent).
    pub check_interval_secs: u64,
    /// Install a release found by the background check without asking.
    pub auto_apply: bool,
    /// Also require a keyless transparency-log signature on every release
    /// file, as a second check after the embedded-key signature.
    pub require_transparency_log: bool,
    /// How many `pre-update` snapshots each profile keeps.
    #[schemars(range(min = 1, max = 100))]
    pub snapshot_keep: u32,
    /// Base URL serving release indexes and archives in place of the
    /// default origin (`<url>/channels/<channel>.json`,
    /// `<url>/<tag>/release-manifest.json`, ...). Everything downloaded from
    /// it is still verified against the embedded release keys.
    pub mirror_url: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 86_400,
            auto_apply: false,
            require_transparency_log: false,
            snapshot_keep: 3,
            mirror_url: None,
        }
    }
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
        let parse_error = |message: &str| {
            Error::new(
                &codes::CONFIG_PARSE,
                format!("{}: {message}", path.display()),
            )
            .with_hint(format!("fix or remove {}", path.display()))
        };
        let config: Self = toml::from_str(&text).map_err(|e| parse_error(e.message()))?;
        if !(1..=100).contains(&config.update.snapshot_keep) {
            return Err(parse_error(
                "update.snapshot_keep: must be between 1 and 100",
            ));
        }
        Ok(config)
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
        assert_eq!(config.update.check_interval_secs, 86_400);
        assert_eq!(config.update.snapshot_keep, 3);
        assert!(!config.update.auto_apply);
    }

    #[test]
    fn snapshot_keep_out_of_range_is_refused() {
        let err = load("[update]\nsnapshot_keep = 0\n").unwrap_err();
        assert_eq!(err.code().code, "config.parse");
        assert!(err.message().contains("update.snapshot_keep"), "{err}");
    }

    #[test]
    fn update_section_is_read() {
        let config =
            load("[update]\ncheck_interval_secs = 0\nmirror_url = \"http://m\"\n").unwrap();
        assert_eq!(config.update.check_interval_secs, 0);
        assert_eq!(config.update.mirror_url.as_deref(), Some("http://m"));
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
