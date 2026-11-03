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
    /// The gateway daemon: its client listener and lifecycle.
    #[serde(default)]
    pub daemon: DaemonSection,
    /// Daemon-wide messaging gateway settings (spec: messaging-gateway,
    /// Profiles served by one daemon).
    #[serde(default)]
    pub gateway: DaemonGateway,
    /// Limits applied to every `prometheus` process of this home.
    #[serde(default)]
    pub process: ProcessConfig,
    /// Self-update of this install (`prometheus update`).
    #[serde(default)]
    pub update: UpdateConfig,
    /// The managed runtime store (`prometheus runtimes`).
    #[serde(default)]
    pub runtimes: RuntimesConfig,
}

/// `daemon.toml [gateway]`: the messaging gateway keys every served
/// profile shares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonGateway {
    /// What the daemon does once every messaging adapter failed for good:
    /// keep serving local clients and schedulers, or exit 78.
    pub on_all_adapters_down: AllAdaptersDown,
    /// The relay connector (`prometheus gateway enroll` writes it).
    pub relay: crate::daemon_relay::RelayConfig,
    /// The shared HTTP listener of webhook-style adapters.
    pub http: GatewayHttp,
    /// How much the daemon runs at once across profiles and surfaces.
    pub limits: GatewayLimits,
}

/// `daemon.toml [gateway.limits]` (spec: messaging-gateway, Concurrency
/// caps across surfaces).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayLimits {
    /// Sessions with a running turn at once, across every profile and
    /// surface; further turns wait first in, first out ("Waiting for a
    /// free slot"). `0`: unbounded.
    pub max_concurrent_sessions: u32,
    /// Detached TUI, desktop and dashboard sessions kept loaded; the least
    /// recently active beyond it are unloaded (never one running a turn or
    /// with a client attached) and resumed from storage when reopened.
    /// `0`: no cap.
    pub max_live_client_sessions: u32,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_concurrent_sessions: 0,
            max_live_client_sessions: 16,
        }
    }
}

/// `daemon.toml [gateway.http]` (spec: platform-adapters, Shared HTTP
/// listener for webhook-style platforms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayHttp {
    /// Where every webhook-style adapter is served, at
    /// `/p/<profile>/<platform>/`.
    pub listen: String,
    /// Allow a non-loopback `listen` address (logged at WARN on every
    /// start).
    pub public: bool,
}

impl Default for GatewayHttp {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8644".to_owned(),
            public: false,
        }
    }
}

impl GatewayHttp {
    /// The parsed listen address.
    ///
    /// # Errors
    ///
    /// The validation message for a malformed address, or a non-loopback
    /// one without `public = true`.
    pub fn listen_addr(&self) -> Result<std::net::SocketAddr, String> {
        let addr: std::net::SocketAddr = self.listen.parse().map_err(|_| {
            format!(
                "gateway.http.listen: '{}' is not an address such as 127.0.0.1:8644",
                self.listen
            )
        })?;
        if addr.ip().is_loopback() || self.public {
            Ok(addr)
        } else {
            Err(format!(
                "gateway.http.listen: '{}' is not a loopback address; set gateway.http.public = true to expose the webhook listener",
                self.listen
            ))
        }
    }
}

/// `gateway.on_all_adapters_down`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AllAdaptersDown {
    /// Keep running (the daemon also serves local clients and schedulers).
    #[default]
    StayAlive,
    /// Stop with exit 78.
    Exit,
}

/// The `[daemon]` section (spec: workspace-architecture, One daemon per
/// Prometheus home; Stale daemon build detection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonSection {
    /// The local client protocol listener.
    pub rpc: RpcListenerConfig,
    /// Seconds `prometheus daemon restart` waits for running turns to reach
    /// a turn boundary before it replaces the daemon anyway.
    pub restart_drain_secs: u64,
    /// Interactive clients (`prometheus tui`, the ACP adapter) start the
    /// daemon detached when it is not running; `false` makes them exit 69
    /// instead.
    pub autostart: bool,
    /// A session with no attached client, no running turn, no open
    /// request and no queued input is closed after this many seconds.
    #[schemars(range(min = 1))]
    pub idle_session_close_secs: u64,
}

impl Default for DaemonSection {
    fn default() -> Self {
        Self {
            rpc: RpcListenerConfig::default(),
            restart_drain_secs: 120,
            autostart: true,
            idle_session_close_secs: 3600,
        }
    }
}

/// `[daemon.rpc]`: the loopback WebSocket every local client attaches to
/// (spec: workspace-architecture, Ticket-authenticated loopback attachment;
/// JSON-RPC framing and error codes; Server-to-client requests; Connection
/// liveness and backpressure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RpcListenerConfig {
    /// Listen address. Loopback only (`127.0.0.1`, `::1`); port `0` picks
    /// a free port, recorded as `rpc_url` in `run/gateway.json`. Remote
    /// access goes through the gateway relay, never this listener.
    pub bind: String,
    /// Largest accepted frame; a larger one closes the connection with 4413.
    #[schemars(range(min = 1024))]
    pub max_frame_bytes: u64,
    /// Requests run concurrently per connection; `rpc.ping` never waits
    /// for one.
    #[schemars(range(min = 1, max = 256))]
    pub workers: u32,
    /// Deadline of server-to-client requests other than approvals (which
    /// use the profile's `approvals.timeout_secs`).
    #[schemars(range(min = 1))]
    pub request_timeout_secs: u64,
}

impl Default for RpcListenerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".to_owned(),
            max_frame_bytes: 16 * 1024 * 1024,
            workers: 8,
            request_timeout_secs: 600,
        }
    }
}

impl RpcListenerConfig {
    /// The parsed listen address.
    ///
    /// # Errors
    ///
    /// The validation message for a malformed or non-loopback address.
    pub fn bind_addr(&self) -> Result<std::net::SocketAddr, String> {
        let addr: std::net::SocketAddr = self.bind.parse().map_err(|_| {
            format!(
                "daemon.rpc.bind: '{}' is not an address such as 127.0.0.1:0",
                self.bind
            )
        })?;
        if addr.ip().is_loopback() {
            Ok(addr)
        } else {
            Err(format!(
                "daemon.rpc.bind: '{}' is not a loopback address; the client listener is loopback-only and remote access goes through the relay (gateway.relay)",
                self.bind
            ))
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.bind_addr()?;
        if self.max_frame_bytes < 1024 {
            return Err("daemon.rpc.max_frame_bytes: must be at least 1024".to_owned());
        }
        if !(1..=256).contains(&self.workers) {
            return Err("daemon.rpc.workers: must be between 1 and 256".to_owned());
        }
        if self.request_timeout_secs == 0 {
            return Err("daemon.rpc.request_timeout_secs: must be at least 1".to_owned());
        }
        Ok(())
    }
}

/// The runtime names of the binary's pinned runtime index (spec:
/// distribution-and-update, Managed runtime store); `runtimes.exclude`
/// accepts only these. `pmt-update` checks its index names the same set.
pub const RUNTIME_NAMES: &[&str] = &["node", "cpython", "uv", "chromium"];

/// The `[runtimes]` section (spec: distribution-and-update, Lazy runtime
/// installation policy; Persistent runtime opt-outs; Download mirrors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimesConfig {
    /// Install a missing pinned runtime the first time a tool needs it
    /// (asking first on an interactive terminal). `false` makes the tool
    /// fail with a `prometheus runtimes install <name>` hint instead.
    pub allow_lazy_install: bool,
    /// Runtimes nothing may install: not lazily, not `prometheus runtimes
    /// install`, not the pin refresh after an update. `install.sh
    /// --without <name>` records names here.
    pub exclude: Vec<String>,
    /// Mirror base URLs used in place of the default origins. Everything
    /// downloaded from a mirror is still checked against the digests pinned
    /// in the binary, so a mirror cannot change what is installed.
    pub mirrors: RuntimeMirrors,
}

impl Default for RuntimesConfig {
    fn default() -> Self {
        Self {
            allow_lazy_install: true,
            exclude: Vec::new(),
            mirrors: RuntimeMirrors::default(),
        }
    }
}

/// `[runtimes.mirrors]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeMirrors {
    /// Runtime archives: `<url>/<name>/<version>/<archive file name>`.
    pub runtimes: Option<String>,
    /// Python package index for the Python bridge's packages (used by the
    /// bridge, milestone M2).
    pub pypi: Option<String>,
    /// Node package registry (used by Node-based plugins, milestone M2).
    pub npm: Option<String>,
}

/// `update.pre_update_backup`: what `prometheus update` saves before the
/// swap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreUpdateBackup {
    /// A `pre-update` snapshot of each profile (config, grants, state.db).
    #[default]
    State,
    /// The snapshots plus a full home archive under `updates/backups/`.
    Full,
    /// No backup; the pre-migration snapshot a rollback needs is still
    /// taken when the new build migrates a store.
    Off,
}

impl PreUpdateBackup {
    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Full => "full",
            Self::Off => "off",
        }
    }
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
#[allow(clippy::struct_excessive_bools)] // independent update switches
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
    /// When a mirror (`update.mirror_url`, `runtimes.mirrors.runtimes`)
    /// still fails after the retry budget, download from the default origin
    /// instead of failing.
    pub mirror_fallback: bool,
    /// What to save before the binary is swapped.
    pub pre_update_backup: PreUpdateBackup,
    /// Let gateway admins run `/update` from a chat.
    pub allow_from_chat: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 86_400,
            auto_apply: false,
            require_transparency_log: false,
            snapshot_keep: 3,
            mirror_url: None,
            mirror_fallback: false,
            pre_update_backup: PreUpdateBackup::State,
            allow_from_chat: false,
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
        config
            .daemon
            .rpc
            .validate()
            .map_err(|message| parse_error(&message))?;
        config
            .gateway
            .relay
            .validate()
            .map_err(|message| parse_error(&message))?;
        config
            .gateway
            .http
            .listen_addr()
            .map_err(|message| parse_error(&message))?;
        if let Some(unknown) = config
            .runtimes
            .exclude
            .iter()
            .find(|name| !RUNTIME_NAMES.contains(&name.as_str()))
        {
            return Err(parse_error(&format!(
                "runtimes.exclude: unknown runtime '{unknown}' (expected {})",
                RUNTIME_NAMES.join("|")
            )));
        }
        Ok(config)
    }
}

/// Replaces `runtimes.exclude` in `<home>/daemon.toml`, keeping the rest
/// of the file (comments included) as it is.
///
/// # Errors
///
/// `config.parse` for an unreadable `daemon.toml` or an unknown name;
/// `home.io` when it cannot be written.
pub fn set_runtime_exclude(home: &Path, names: &[String]) -> Result<(), Error> {
    let path = home.join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Error::new(
                &codes::HOME_IO,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {}", path.display(), e.message()),
        )
    })?;
    let section = doc
        .entry("runtimes")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| {
            Error::new(
                &codes::CONFIG_PARSE,
                format!("{}: runtimes is not a table", path.display()),
            )
        })?;
    section["exclude"] = toml_edit::value(names.iter().collect::<toml_edit::Array>());
    let updated = doc.to_string();
    // Validate before writing: a bad name never reaches the file.
    let config: DaemonConfig = toml::from_str(&updated).map_err(|e| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {}", path.display(), e.message()),
        )
    })?;
    if let Some(unknown) = config
        .runtimes
        .exclude
        .iter()
        .find(|n| !RUNTIME_NAMES.contains(&n.as_str()))
    {
        return Err(Error::new(
            &codes::RUNTIME_UNKNOWN,
            format!(
                "unknown runtime '{unknown}' (expected {})",
                RUNTIME_NAMES.join("|")
            ),
        ));
    }
    pmt_core::private_fs::write_atomic(&path, updated.as_bytes()).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write {}: {e}", path.display()),
        )
    })
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
    fn runtimes_default_to_lazy_installs_with_nothing_excluded() {
        let home = tempfile::tempdir().unwrap();
        let config = DaemonConfig::load(home.path()).unwrap();
        assert!(config.runtimes.allow_lazy_install);
        assert!(config.runtimes.exclude.is_empty());
        assert_eq!(
            config.update.pre_update_backup,
            super::PreUpdateBackup::State
        );
        assert!(!config.update.mirror_fallback);
    }

    #[test]
    fn unknown_excluded_runtime_fails_validation() {
        let err = load("[runtimes]\nexclude = [\"chromium\", \"java\"]\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message().contains(
                "runtimes.exclude: unknown runtime 'java' (expected node|cpython|uv|chromium)"
            ),
            "{err}"
        );
    }

    #[test]
    fn exclude_edits_keep_the_rest_of_the_file() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("daemon.toml");
        std::fs::write(&path, "# mine\n[update]\nsnapshot_keep = 5\n").unwrap();
        super::set_runtime_exclude(home.path(), &["chromium".to_owned()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        let config = DaemonConfig::load(home.path()).unwrap();
        assert_eq!(config.runtimes.exclude, ["chromium"]);
        assert_eq!(config.update.snapshot_keep, 5);
        let err = super::set_runtime_exclude(home.path(), &["java".to_owned()]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            DaemonConfig::load(home.path()).unwrap().runtimes.exclude,
            ["chromium"]
        );
    }

    #[test]
    fn public_rpc_bind_is_refused_naming_the_relay() {
        let err = load("[daemon.rpc]\nbind = \"0.0.0.0:7777\"\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message().contains(
                "the client listener is loopback-only and remote access goes through the relay"
            ),
            "{err}"
        );
        let config = load("[daemon.rpc]\nbind = \"[::1]:0\"\n").unwrap();
        assert!(config.daemon.rpc.bind_addr().unwrap().ip().is_loopback());
        assert_eq!(config.daemon.rpc.workers, 8);
        assert_eq!(config.daemon.restart_drain_secs, 120);
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
