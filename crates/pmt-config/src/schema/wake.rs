//! The `[wake]` section (spec: voice-mode, Wake word listener; Wake
//! engines and false-trigger control; Single wake listener ownership
//! across surfaces; Wake capture location for remote clients; Wake word
//! profile routing): an on-device wake word, off by default.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[wake]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
#[allow(clippy::struct_excessive_bools)] // independent user switches
pub struct WakeConfig {
    /// Listen for the wake phrase (`/wake on|off` writes it).
    pub enabled: bool,
    /// A wake opens a new session; off continues the current one.
    pub start_new_session: bool,
    /// The wake phrase: what `sherpa` listens for, a label for the
    /// model-keyed engines.
    pub phrase: String,
    /// The detection engine; `auto` takes the first one this host
    /// supports (`openwakeword`, `sherpa`, `porcupine`).
    pub engine: WakeEngine,
    /// Stricter when higher, for every engine.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub sensitivity: f64,
    /// Consecutive over-threshold frames before `openwakeword` fires.
    #[schemars(range(min = 1, max = 10))]
    pub confirmation_frames: u32,
    /// Which local surface owns the microphone (`auto`: the first to arm).
    pub surface: WakeSurface,
    /// Where the wake audio is captured.
    pub capture: WakeCapture,
    /// With `sherpa`, also listen for the phrases of the other profiles
    /// with wake enabled.
    pub profile_routing: bool,
    /// The input device: an index or an unambiguous name substring
    /// (unset: the default device).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_device: Option<String>,
    /// `openwakeword` settings.
    pub openwakeword: OpenWakeWordConfig,
    /// `sherpa` settings.
    pub sherpa: SherpaConfig,
    /// `porcupine` settings (access key: secret `PORCUPINE_ACCESS_KEY`).
    pub porcupine: PorcupineConfig,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_new_session: true,
            phrase: "hey prometheus".to_owned(),
            engine: WakeEngine::Auto,
            sensitivity: 0.6,
            confirmation_frames: 3,
            surface: WakeSurface::Auto,
            capture: WakeCapture::Auto,
            profile_routing: true,
            input_device: None,
            openwakeword: OpenWakeWordConfig::default(),
            sherpa: SherpaConfig::default(),
            porcupine: PorcupineConfig::default(),
        }
    }
}

/// `wake.engine`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WakeEngine {
    /// The first supported engine, in the order below.
    #[default]
    Auto,
    /// openWakeWord models (free, on-device).
    Openwakeword,
    /// sherpa-onnx keyword spotting (any typed phrase).
    Sherpa,
    /// Picovoice Porcupine (needs an access key).
    Porcupine,
}

impl WakeEngine {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Openwakeword => "openwakeword",
            Self::Sherpa => "sherpa",
            Self::Porcupine => "porcupine",
        }
    }
}

/// `wake.surface`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WakeSurface {
    /// The first eligible local surface to arm.
    #[default]
    Auto,
    /// The line interface.
    Cli,
    /// The terminal UI.
    Tui,
    /// The desktop app.
    Gui,
}

impl WakeSurface {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Gui => "gui",
        }
    }
}

/// `wake.capture`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WakeCapture {
    /// Client capture when the desktop arms with `client_capture`, else
    /// local.
    #[default]
    Auto,
    /// The microphone of the host running the engine.
    Local,
    /// PCM frames an attached client sends (`wake.feed`).
    Client,
}

impl WakeCapture {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Local => "local",
            Self::Client => "client",
        }
    }
}

/// `[wake.openwakeword]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OpenWakeWordConfig {
    /// A built-in model name or an absolute `.onnx`/`.tflite` path.
    pub model: String,
}

impl Default for OpenWakeWordConfig {
    fn default() -> Self {
        Self {
            model: "hey_jarvis".to_owned(),
        }
    }
}

/// `[wake.sherpa]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SherpaConfig {
    /// A keyword-spotting model directory (unset: the default model,
    /// downloaded once into the profile's models directory).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
}

/// `[wake.porcupine]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PorcupineConfig {
    /// A built-in keyword or an absolute `.ppn` path.
    pub keyword: String,
}

impl Default for PorcupineConfig {
    fn default() -> Self {
        Self {
            keyword: "jarvis".to_owned(),
        }
    }
}
