//! The `[bot_desktop]` section (spec: execution-environments, Bot screen;
//! Bot screen takeover lease): the per-profile virtual display the agent's
//! headed browsers and `computer_use` act on.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[bot_desktop]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct BotDesktopConfig {
    /// The screen's size, `<width>x<height>` in pixels.
    #[schemars(regex(pattern = r"^[1-9][0-9]{2,4}x[1-9][0-9]{2,4}$"))]
    pub geometry: String,
    /// Start the screen on the first `computer_use` call or headed browser
    /// launch instead of only on `prometheus computer-use screen start`.
    pub auto_start: bool,
    /// The screen refuses to start with less free memory than this on the
    /// host or its cgroup (`0`: no check).
    pub min_free_memory_bytes: u64,
    /// The screen stops after this long without use (`0`: never).
    pub idle_stop_secs: u64,
    /// Where the screen runs.
    pub placement: ScreenPlacement,
    /// The image of a screen placed in the container backend (empty: the
    /// release's desktop sandbox image, `prometheus-sandbox:<version>-desktop`).
    pub image: String,
}

impl Default for BotDesktopConfig {
    fn default() -> Self {
        Self {
            geometry: "1440x900".to_owned(),
            auto_start: false,
            min_free_memory_bytes: 1_610_612_736,
            idle_stop_secs: 1800,
            placement: ScreenPlacement::Auto,
            image: String::new(),
        }
    }
}

impl BotDesktopConfig {
    /// The geometry as `(width, height)`, when well formed.
    #[must_use]
    pub fn size(&self) -> Option<(u32, u32)> {
        let (width, height) = self.geometry.split_once('x')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    }
}

/// Where the bot screen runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenPlacement {
    /// Inside a container or SSH terminal backend, on the gateway host for
    /// `local`; refused for backends that cannot host a display.
    Auto,
    /// Always where the terminal runs (refused when it cannot host one).
    Terminal,
    /// On the gateway host whatever the terminal backend: the agent's
    /// screen then acts outside the terminal sandbox (explicit opt-in).
    Gateway,
}

impl ScreenPlacement {
    /// The config value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Terminal => "terminal",
            Self::Gateway => "gateway",
        }
    }
}
