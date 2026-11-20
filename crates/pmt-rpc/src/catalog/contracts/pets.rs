//! Pets and achievements (spec: pets): what the desktop and the dashboard
//! read and change of a profile's pets, the generate flow, the
//! achievements snapshot and the unlock event. Nothing here touches a
//! session.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method};

/// A profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetsProfileParams {
    /// The profile.
    pub profile: String,
}

/// One installed or gallery pet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetInfo {
    /// Its slug.
    pub slug: String,
    /// Its display name.
    pub name: String,
    /// Who made it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// Installed in the profile.
    pub installed: bool,
}

/// `pets.state` result: the `[pets]` keys and the installed pets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetsState {
    /// `pets.enabled`.
    pub enabled: bool,
    /// The pet shown (the configured one if installed, else the first).
    pub active: Option<String>,
    /// `pets.scale`.
    pub scale: f64,
    /// `pets.roam`.
    pub roam: bool,
    /// The installed pets.
    pub installed: Vec<PetInfo>,
    /// The line to show for the change just made, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// `pets.sprite` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetSpriteParams {
    /// The profile.
    pub profile: String,
    /// The pet (default: the active one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

/// `pets.sprite` result: the validated atlas and each state's row and
/// frame count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetSprite {
    /// The pet.
    pub slug: String,
    /// Its name.
    pub name: String,
    /// The atlas as a `data:` URL (PNG or WebP).
    pub atlas: String,
    /// A cell's width in pixels.
    pub frame_width: u32,
    /// A cell's height in pixels.
    pub frame_height: u32,
    /// The atlas row and the frame count of each state (`idle`, `wave`,
    /// `run`, `failed`, `review`, `jump`, `waiting`).
    pub states: BTreeMap<String, PetRow>,
}

/// One state's row of the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetRow {
    /// The row.
    pub row: u32,
    /// Its frames (0: the state shows the idle row).
    pub frames: u32,
}

/// `pets.gallery` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetGalleryParams {
    /// The profile (its `pets.gallery_url`).
    pub profile: String,
    /// Only pets whose slug or name contains this.
    #[serde(default)]
    pub query: String,
    /// At most this many (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `pets.gallery` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetGallery {
    /// The gallery's pets.
    pub pets: Vec<PetInfo>,
}

/// `pets.install` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetInstallParams {
    /// The profile.
    pub profile: String,
    /// The gallery slug.
    pub slug: String,
    /// Show it from now on.
    #[serde(default)]
    pub select: bool,
}

/// `pets.set` params: the `[pets]` keys to change (validated; the scale is
/// clamped to 0.1-3.0).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PetSetParams {
    /// The profile.
    pub profile: String,
    /// Show or hide the pet (on without a slug: the first installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Adopt this installed pet (and show it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The size on every surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Wander while the agent is idle (desktop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roam: Option<bool>,
}

/// `pets.hatch.start` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HatchStartParams {
    /// The profile.
    pub profile: String,
    /// What the pet looks like.
    pub description: String,
    /// The session whose usage ledger the paid image calls go to (the
    /// chat `/hatch` was typed in); absent, a hidden session is made for
    /// the flow on its first paid call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// `pets.hatch.answer` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HatchAnswerParams {
    /// The profile.
    pub profile: String,
    /// The flow `pets.hatch.start` opened.
    pub flow: String,
    /// The answer (`y`, a draft number, `remix <tweak>`, `retry`,
    /// `cancel`).
    pub answer: String,
}

/// One step of a generate flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HatchStep {
    /// The flow.
    pub flow: String,
    /// What to show (a question while `done` is false).
    pub text: String,
    /// The flow is over.
    pub done: bool,
    /// It failed.
    #[serde(default)]
    pub failed: bool,
    /// The drafts to pick from, as PNG `data:` URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drafts: Vec<String>,
    /// The pet installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

/// One badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AchievementBadge {
    /// The achievement id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Exactly what it counts.
    pub description: String,
    /// Its group.
    pub category: String,
    /// The highest tier unlocked (`copper`, `silver`, `gold`, `diamond`,
    /// `olympian`).
    pub tier: Option<String>,
    /// The current count.
    pub value: u64,
    /// The next tier.
    pub next_tier: Option<String>,
    /// The count it needs.
    pub next_threshold: Option<u64>,
    /// When the shown tier unlocked (RFC 3339).
    pub unlocked_at: Option<String>,
}

/// `achievements.list` result: what is known now; a scan runs in the
/// background while `scanning`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Achievements {
    /// The badges.
    pub badges: Vec<AchievementBadge>,
    /// Sessions analysed so far.
    pub sessions: usize,
    /// When the last scan finished.
    pub scanned_at: Option<String>,
    /// A scan runs.
    pub scanning: bool,
}

/// `achievement.unlocked` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AchievementUnlocked {
    /// The profile.
    pub profile: String,
    /// The achievement.
    pub id: String,
    /// The tier reached.
    pub tier: String,
}

method!(/// `pets.state`: the profile's pet settings and installed pets.
    PetsStateGet = "pets.state" (PetsProfileParams) -> PetsState);
method!(/// `pets.sprite`: a pet's validated atlas for drawing.
    PetsSpriteGet = "pets.sprite" (PetSpriteParams) -> PetSprite);
method!(/// `pets.gallery`: the gallery's pets (`pets.gallery_url`).
    PetsGalleryList = "pets.gallery" (PetGalleryParams) -> PetGallery);
method!(/// `pets.install`: downloads, validates and installs a gallery pet.
    PetsInstall = "pets.install" (PetInstallParams) -> PetsState);
method!(/// `pets.set`: a validated patch of the `[pets]` keys.
    PetsSet = "pets.set" (PetSetParams) -> PetsState);
method!(/// `pets.hatch.start`: opens a generate flow (refused with
    /// `pet.no_image_backend`, spending nothing, when no backend takes
    /// reference images).
    PetsHatchStart = "pets.hatch.start" (HatchStartParams) -> HatchStep);
method!(/// `pets.hatch.answer`: answers the flow's question.
    PetsHatchAnswer = "pets.hatch.answer" (HatchAnswerParams) -> HatchStep);
method!(/// `achievements.list`: the badges now; starts an incremental scan in
    /// the background when none runs.
    AchievementsList = "achievements.list" (PetsProfileParams) -> Achievements);
event!(/// `achievement.unlocked`: a tier was reached (never sent to a
    /// model).
    AchievementUnlockedEvent = "achievement.unlocked" (AchievementUnlocked));
