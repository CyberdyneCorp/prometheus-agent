//! The `[computer_use]` section (spec: computer-use): the desktop driver
//! the `computer_use` tool speaks to, the permission mode its runtime is
//! launched in and the capture budgets.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The built-in computer-use backend: the pinned `cua-driver` runtime.
pub const CUA_BACKEND: &str = "cua";

/// The `[computer_use]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
#[allow(clippy::struct_excessive_bools)] // independent switches
pub struct ComputerUseConfig {
    /// The computer-use backend: `cua` (the pinned `cua-driver`) or a
    /// backend an installed plugin registers.
    pub backend: String,
    /// The driver runtime's permission mode: `standard` (the driver asks
    /// through Prometheus' approvals) or `bounded` (a reviewed capability
    /// manifest is the boundary). `unrestricted` is never a config value:
    /// only `/yolo` launches it, for that session.
    pub permission_mode: ComputerUsePermission,
    /// The reviewed capability manifest a `bounded` runtime is launched
    /// with (required for `bounded`).
    pub capability_manifest: Option<PathBuf>,
    /// A driver binary to use instead of the managed `cua-driver` runtime
    /// (never repaired or replaced).
    pub driver_path: Option<PathBuf>,
    /// Launch private runtimes through a driver bundle that is not signed
    /// by the official identity (macOS; local driver builds only).
    pub allow_unsigned_driver: bool,
    /// Leave the driver's own telemetry on (it is disabled in the
    /// driver's environment otherwise).
    pub driver_telemetry: bool,
    /// The longest image edge of a capture, in pixels (`0`: no cap).
    pub max_image_dimension: u32,
    /// Bound on the driver's accessibility walk per capture (`0`: the
    /// driver's default). At most 100 elements reach the model; the rest
    /// are written to a session cache file named in the result.
    pub ax_max_elements: u32,
    /// The capture mode of `capture_after`.
    pub capture_after_mode: CaptureMode,
    /// Use native Wayland input on Linux instead of `XWayland`.
    pub native_wayland: bool,
    /// Register a per-logon driver task on Windows so a service session
    /// (Session 0) can drive the interactive desktop.
    pub autostart: bool,
    /// The driver's agent cursor overlay: `auto` (off on macOS, headless
    /// or WSL Linux, containers and Linux X11), `on` or `off`.
    pub overlay: Overlay,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            backend: CUA_BACKEND.to_owned(),
            permission_mode: ComputerUsePermission::Standard,
            capability_manifest: None,
            driver_path: None,
            allow_unsigned_driver: false,
            driver_telemetry: false,
            max_image_dimension: 1456,
            ax_max_elements: 200,
            capture_after_mode: CaptureMode::Som,
            native_wayland: false,
            autostart: false,
            overlay: Overlay::Auto,
        }
    }
}

/// The permission modes config may choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUsePermission {
    /// The driver's actions pass Prometheus' approval gate.
    Standard,
    /// A reviewed `computer_use.capability_manifest` bounds the runtime.
    Bounded,
}

/// What a capture returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// A screenshot with numbered element overlays plus the elements.
    Som,
    /// Pixels only.
    Vision,
    /// Accessibility elements only, no image.
    Ax,
}

impl CaptureMode {
    /// The config and tool argument value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Som => "som",
            Self::Vision => "vision",
            Self::Ax => "ax",
        }
    }

    /// The mode named `text`, if any.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "som" => Some(Self::Som),
            "vision" => Some(Self::Vision),
            "ax" => Some(Self::Ax),
            _ => None,
        }
    }
}

/// The agent cursor overlay setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Overlay {
    /// Decided by the platform.
    Auto,
    /// Always shown.
    On,
    /// Never shown.
    Off,
}

#[cfg(test)]
mod tests {
    use super::{ComputerUseConfig, ComputerUsePermission};

    #[test]
    fn unrestricted_is_not_a_permission_mode() {
        let err = toml::from_str::<ComputerUseConfig>("permission_mode = \"unrestricted\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant"), "{err}");
        let bounded: ComputerUseConfig = toml::from_str("permission_mode = \"bounded\"").unwrap();
        assert_eq!(bounded.permission_mode, ComputerUsePermission::Bounded);
    }
}
