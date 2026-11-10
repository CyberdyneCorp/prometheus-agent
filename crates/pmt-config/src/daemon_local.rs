//! `daemon.toml [local_runtime]` (spec: local-models, Local runtime
//! configuration): the machine-wide llama.cpp runtime. It lives in
//! `daemon.toml` because one inference server is shared by every profile
//! of a home; a profile picks a local model with the ordinary
//! `model.provider = "llamacpp"` and `model.default`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The most models `models_max` may keep resident.
pub const MODELS_MAX_LIMIT: u32 = 16;

/// `local_runtime.backend`: the llama.cpp build to install and run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocalBackend {
    /// Chosen from the probed hardware.
    #[default]
    Auto,
    /// NVIDIA GPUs.
    Cuda,
    /// Apple GPUs (macOS).
    Metal,
    /// AMD and Intel GPUs.
    Vulkan,
    /// AMD GPUs through ROCm (only when configured, `x86_64` only).
    Hip,
    /// No GPU.
    Cpu,
}

impl LocalBackend {
    /// Every backend, in config order.
    pub const ALL: [Self; 6] = [
        Self::Auto,
        Self::Cuda,
        Self::Metal,
        Self::Vulkan,
        Self::Hip,
        Self::Cpu,
    ];

    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cuda => "cuda",
            Self::Metal => "metal",
            Self::Vulkan => "vulkan",
            Self::Hip => "hip",
            Self::Cpu => "cpu",
        }
    }
}

impl std::fmt::Display for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[local_runtime]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRuntimeConfig {
    /// Let Prometheus start and supervise a `llama-server` when a session
    /// needs one. `false` (the default) only detects a running server.
    pub enabled: bool,
    /// The engine build: `auto` picks one from the probed hardware.
    pub backend: LocalBackend,
    /// Models kept resident at once.
    #[schemars(range(min = 1, max = 16))]
    pub models_max: u32,
    /// Loopback port of the managed server; `0` picks a free one at start.
    pub port: u16,
    /// Ports probed for a user-run llama-server besides 8080.
    pub detect_ports: Vec<u16>,
    /// Seconds without a request after which a resident model is unloaded
    /// (it is reloaded by the next request).
    pub idle_unload_secs: u64,
}

impl Default for LocalRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: LocalBackend::Auto,
            models_max: 4,
            port: 0,
            detect_ports: Vec::new(),
            idle_unload_secs: 900,
        }
    }
}

impl LocalRuntimeConfig {
    /// Range checks serde cannot express.
    ///
    /// # Errors
    ///
    /// The offending key and its accepted range.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MODELS_MAX_LIMIT).contains(&self.models_max) {
            return Err(format!(
                "local_runtime.models_max: must be between 1 and {MODELS_MAX_LIMIT}"
            ));
        }
        if self.detect_ports.contains(&0) {
            return Err("local_runtime.detect_ports: 0 is not a port".to_owned());
        }
        Ok(())
    }
}

/// Sets `local_runtime.enabled` in `<home>/daemon.toml`, keeping the rest
/// of the file (comments included) as it is.
///
/// # Errors
///
/// `config.parse` for an unreadable `daemon.toml`; `home.io` when it
/// cannot be written.
pub fn set_enabled(home: &std::path::Path, enabled: bool) -> Result<(), pmt_core::Error> {
    use pmt_core::error::codes;
    let path = home.join(crate::daemon::FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(pmt_core::Error::io(&codes::HOME_IO, &path, &e)),
    };
    let parse = |message: &str| {
        pmt_core::Error::new(
            &codes::CONFIG_PARSE,
            format!("{}: {message}", path.display()),
        )
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| parse(e.message()))?;
    let section = doc
        .entry("local_runtime")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| parse("local_runtime is not a table"))?;
    section["enabled"] = toml_edit::value(enabled);
    let updated = doc.to_string();
    toml::from_str::<crate::daemon::DaemonConfig>(&updated).map_err(|e| parse(e.message()))?;
    pmt_core::private_fs::write_atomic(&path, updated.as_bytes())
        .map_err(|e| pmt_core::Error::io(&codes::HOME_IO, &path, &e))
}

#[cfg(test)]
mod tests {
    use crate::daemon::DaemonConfig;

    fn load(text: &str) -> Result<DaemonConfig, pmt_core::Error> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::daemon::FILE_NAME), text).unwrap();
        DaemonConfig::load(dir.path())
    }

    #[test]
    fn enabling_keeps_the_rest_of_daemon_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::daemon::FILE_NAME);
        std::fs::write(&path, "# mine\n[local_runtime]\nport = 9000\n").unwrap();
        super::set_enabled(dir.path(), true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        let config = DaemonConfig::load(dir.path()).unwrap().local_runtime;
        assert!(config.enabled);
        assert_eq!(config.port, 9000);
    }

    #[test]
    fn the_default_only_detects() {
        let config = load("").unwrap().local_runtime;
        assert!(!config.enabled);
        assert_eq!(config.backend, super::LocalBackend::Auto);
        assert_eq!(config.models_max, 4);
        assert_eq!(config.port, 0);
        assert!(config.detect_ports.is_empty());
        assert_eq!(config.idle_unload_secs, 900);
    }

    #[test]
    fn an_invalid_backend_lists_the_accepted_ones_and_exits_78() {
        let err = load("[local_runtime]\nbackend = \"rocm\"\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        for backend in super::LocalBackend::ALL {
            assert!(
                err.message().contains(&format!("`{backend}`")),
                "{}",
                err.message()
            );
        }
    }

    #[test]
    fn unknown_keys_and_out_of_range_values_are_refused() {
        for text in [
            "[local_runtime]\nengine_version = \"b1\"\n",
            "[local_runtime]\nmodels_max = 0\n",
            "[local_runtime]\nmodels_max = 17\n",
            "[local_runtime]\ndetect_ports = [0]\n",
            "[local_runtime]\nport = 70000\n",
        ] {
            let err = load(text).unwrap_err();
            assert_eq!(err.exit_code(), 78, "{text}");
        }
        let ok = load("[local_runtime]\nenabled = true\nmodels_max = 16\ndetect_ports = [8081]\n")
            .unwrap()
            .local_runtime;
        assert!(ok.enabled);
        assert_eq!(ok.detect_ports, [8081]);
    }
}
