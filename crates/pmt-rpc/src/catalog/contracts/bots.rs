//! Bot Mode (spec: bot-mode): bot metadata kept on the backend so every
//! client shows the same bot, creating, duplicating and deleting bots,
//! a bot's routines, and the roster's read marker.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Method;

/// The blob face's silhouettes.
pub const SILHOUETTES: [&str; 6] = ["round", "organic", "boxy", "nub", "cloud", "sun"];
/// The geometric avatar's shapes.
pub const SHAPES: [&str; 7] = [
    "circle", "square", "triangle", "diamond", "hexagon", "star", "heart",
];
/// The geometric avatar's colours.
pub const COLOURS: [&str; 10] = [
    "red", "orange", "amber", "green", "teal", "cyan", "blue", "indigo", "purple", "pink",
];

/// A bot's avatar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BotAvatar {
    /// The default soft-body face, derived from the bot's name unless a
    /// seed is locked.
    Blob {
        /// A locked seed (the name's own seed otherwise).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seed: Option<u32>,
        /// One of [`SILHOUETTES`] (derived from the seed otherwise).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        silhouette: Option<String>,
    },
    /// One of 7 shapes in one of 10 colours.
    Geometric {
        /// One of [`SHAPES`].
        shape: String,
        /// One of [`COLOURS`].
        color: String,
    },
    /// An uploaded image, stored content-addressed in the profile's blobs.
    Image {
        /// The image's SHA-256 (set by the daemon).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
        /// `image/png`, `image/jpeg` or `image/webp`.
        mime: String,
        /// Base64 bytes: upload only, never returned (`bot.avatar.get`
        /// reads the picture).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// An image made by the profile's image-generation backend (update
    /// only: it is stored as `image`).
    Generated {
        /// What to draw.
        prompt: String,
    },
    /// A pets-gallery companion.
    Pet {
        /// The companion id.
        id: String,
    },
}

impl Default for BotAvatar {
    fn default() -> Self {
        Self::Blob {
            seed: None,
            silhouette: None,
        }
    }
}

/// A roster section heading, stored on each member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotSection {
    /// Stable id.
    pub id: String,
    /// Heading.
    pub name: String,
}

/// A bot's metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BotMeta {
    /// The bot's title (the profile name when unset).
    pub title: Option<String>,
    /// What it does.
    pub description: Option<String>,
    /// Its avatar.
    pub avatar: BotAvatar,
    /// Left out of the default roster and toasts (still reachable).
    pub hidden: bool,
    /// Its roster section.
    pub section: Option<BotSection>,
    /// The rooms it is a member of (read only).
    pub groups: Vec<String>,
}

/// `bot.meta.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotMetaParams {
    /// The bot.
    pub profile: String,
}

/// `bot.meta.update` params: each key of `changes` (`title`,
/// `description`, `avatar`, `hidden`, `section`) replaces that field;
/// `null` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotMetaUpdateParams {
    /// The bot.
    pub profile: String,
    /// The fields to change.
    pub changes: Map<String, Value>,
}

/// `bot.create` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BotCreateParams {
    /// The profile asking (the window's): it must be granted.
    pub profile: String,
    /// The new bot's profile name.
    pub name: String,
    /// Its title.
    pub title: Option<String>,
    /// What it does.
    pub description: Option<String>,
    /// Clone config, skills, `SOUL.md` and memory from this profile.
    pub clone_from: Option<String>,
    /// No bundled skills.
    pub empty: bool,
    /// Pin this model (`provider/model` or a model name).
    pub model: Option<String>,
    /// The bot's `SOUL.md`.
    pub soul: Option<String>,
    /// Skills to enable (the others stay as the profile has them).
    pub skills: Vec<String>,
    /// `tools.enabled_toolsets`.
    pub toolsets: Vec<String>,
    /// MCP servers to keep enabled.
    pub mcp_servers: Vec<String>,
    /// Copy the source's static API keys (never OAuth or single-use
    /// logins); default true.
    pub copy_api_keys: Option<bool>,
    /// A registered peer (or desktop connection) to create the bot on.
    pub on: Option<String>,
}

/// `bot.create` / `bot.duplicate` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotCreated {
    /// The new bot's profile.
    pub profile: String,
    /// Its roster handle.
    pub handle: String,
    /// Its canonical Bot Chat.
    pub session_id: String,
    /// The machine it lives on (`local` or the peer).
    pub device: String,
    /// The secrets copied from the source.
    #[serde(default)]
    pub copied_secrets: Vec<String>,
    /// Its introduction (the Bot Chat's first message).
    #[serde(default)]
    pub introduction: Option<String>,
}

/// `bot.duplicate` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotDuplicateParams {
    /// The bot to duplicate.
    pub profile: String,
    /// The copy's profile name.
    pub name: String,
}

/// `bot.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotDeleteParams {
    /// The bot.
    pub profile: String,
    /// Must be true.
    #[serde(default)]
    pub confirm: bool,
}

/// `bot.delete` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotDeleted {
    /// The deleted profile.
    pub profile: String,
}

/// One routine: a scheduled job of the bot named `[bot:<name>] <routine>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotRoutine {
    /// The job id (`prometheus -p <bot> cron ...` takes it).
    pub job_id: String,
    /// The routine's name (after the `[bot:<name>]` prefix).
    pub routine: String,
    /// The schedule string.
    pub schedule: String,
    /// `active`, `paused`, ...
    pub state: String,
    /// The next run (RFC 3339).
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// How the last run ended.
    #[serde(default)]
    pub last_status: Option<String>,
}

/// `bot.routines.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotRoutines {
    /// The bot's routines.
    pub routines: Vec<BotRoutine>,
}

/// `bot.sections.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotSectionDeleteParams {
    /// The profile asking (the window's).
    pub profile: String,
    /// The section.
    pub section_id: String,
}

/// `bot.sections.delete` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotSectionDeleted {
    /// The bots moved to unassigned.
    pub moved: Vec<String>,
}

/// An empty result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotDone {}

/// `voice.speak` params: read a message of a session aloud.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceSpeakParams {
    /// The profile that owns the session (a bot speaks with its own
    /// voice).
    pub profile: String,
    /// The session the text belongs to.
    pub session_id: String,
    /// What to say.
    pub text: String,
}

/// `bot.avatar.get` result: the picture of an `image` avatar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotAvatarPicture {
    /// `image/png`, `image/jpeg` or `image/webp`.
    pub mime: String,
    /// The image, base64.
    pub data: String,
}

/// `voice.speak` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceSpoken {
    /// The audio's media type.
    pub mime: String,
    /// The audio, base64.
    pub audio: String,
}

method!(/// `bot.meta.get`: the bot's metadata.
    BotMetaGet = "bot.meta.get" (BotMetaParams) -> BotMeta);
method!(/// `bot.avatar.get`: the picture of the bot's `image` avatar (an
    /// uploaded or generated one), which the roster names only by blob.
    BotAvatarGet = "bot.avatar.get" (BotMetaParams) -> BotAvatarPicture);
method!(/// `bot.meta.update`: changes the bot's metadata (an unknown avatar
    /// kind is rejected).
    BotMetaUpdate = "bot.meta.update" (BotMetaUpdateParams) -> BotMeta);
method!(/// `bot.create`: creates a bot (a profile), its canonical Bot Chat
    /// and its introduction.
    BotCreate = "bot.create" (BotCreateParams) -> BotCreated);
method!(/// `bot.duplicate`: copies a bot's config, skills, `SOUL.md`,
    /// memory and metadata.
    BotDuplicate = "bot.duplicate" (BotDuplicateParams) -> BotCreated);
method!(/// `bot.delete`: deletes the bot's profile (never `default`).
    BotDelete = "bot.delete" (BotDeleteParams) -> BotDeleted);
method!(/// `bot.routines.list`: the bot's routines.
    BotRoutinesList = "bot.routines.list" (BotMetaParams) -> BotRoutines);
method!(/// `bot.chat.read`: marks the bot's Bot Chat read (the roster's
    /// unread count starts again from zero).
    BotChatRead = "bot.chat.read" (BotMetaParams) -> BotDone);
method!(/// `bot.sections.delete`: moves the section's bots to unassigned.
    BotSectionsDelete = "bot.sections.delete" (BotSectionDeleteParams) -> BotSectionDeleted);
method!(/// `voice.speak`: synthesizes text of a session with the `tts.*`
    /// settings of the profile that owns it (a bot's own voice).
    VoiceSpeak = "voice.speak" (VoiceSpeakParams) -> VoiceSpoken);
