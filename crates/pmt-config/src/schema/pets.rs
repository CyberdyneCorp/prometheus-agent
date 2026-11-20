//! The `[pets]` section (spec: pets, Pets are cosmetic and cache-neutral;
//! Pet configuration and scale): an optional animated mascot, off by
//! default. Nothing here reaches a session: every key applies live and
//! only to how clients draw the pet.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The public petdex gallery.
pub const DEFAULT_GALLERY_URL: &str = "https://petdex.dev/api/manifest";
/// `pets.scale` when unset: a third of the native 192x208 cell.
pub const DEFAULT_PET_SCALE: f64 = 0.33;
/// The smallest `pets.scale` (the pet stays visible and clickable).
pub const MIN_PET_SCALE: f64 = 0.1;
/// The largest `pets.scale` (a mistyped value never fills the screen).
pub const MAX_PET_SCALE: f64 = 3.0;

/// The `[pets]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct PetsConfig {
    /// Show the pet on the line interface, the TUI and the desktop app.
    pub enabled: bool,
    /// The installed pet shown (empty: the first installed one).
    pub slug: String,
    /// How terminals draw it (`auto` picks the richest graphics protocol
    /// the terminal speaks).
    pub render_mode: PetRenderMode,
    /// The size, as a fraction of the native 192x208 pixel cell, on every
    /// surface.
    #[schemars(range(min = 0.1, max = 3.0))]
    pub scale: f64,
    /// Terminal columns of a half-block pet (0: derived from `scale`).
    #[schemars(range(min = 0, max = 200))]
    pub unicode_cols: u32,
    /// On the desktop, the pet wanders while the agent is idle.
    pub roam: bool,
    /// The image backend `/hatch` uses (empty: the first configured one
    /// that takes reference images).
    pub image_provider: String,
    /// The gallery `prometheus pets list` and `install` read.
    pub gallery_url: String,
}

impl Default for PetsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slug: String::new(),
            render_mode: PetRenderMode::Auto,
            scale: DEFAULT_PET_SCALE,
            unicode_cols: 0,
            roam: false,
            image_provider: String::new(),
            gallery_url: DEFAULT_GALLERY_URL.to_owned(),
        }
    }
}

/// `pets.render_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PetRenderMode {
    /// The richest protocol the terminal is known to speak, else
    /// half-blocks.
    #[default]
    Auto,
    /// The kitty graphics protocol (kitty, `Ghostty`, `WezTerm`).
    Kitty,
    /// iTerm2 inline images.
    Iterm,
    /// DEC sixel graphics.
    Sixel,
    /// Truecolor Unicode half-blocks.
    Unicode,
    /// Never draw the pet in a terminal.
    Off,
}

impl PetRenderMode {
    /// The config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm => "iterm",
            Self::Sixel => "sixel",
            Self::Unicode => "unicode",
            Self::Off => "off",
        }
    }

    /// The mode a config spelling names.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Auto,
            Self::Kitty,
            Self::Iterm,
            Self::Sixel,
            Self::Unicode,
            Self::Off,
        ]
        .into_iter()
        .find(|m| m.as_str() == text)
    }
}
