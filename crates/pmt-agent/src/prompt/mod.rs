//! The layered system prompt (spec: agent-turn-loop, Layered system
//! prompt; Identity file independent of context files; System prompt
//! frozen per session).
//!
//! The prompt is three tiers, each sent as one system block ending at a
//! cache breakpoint: `static` (identity, tool-use and model-family
//! guidance), `profile` (platform hint, skills index, pinned skills) and
//! `session` (personality, caller system message, project context, memory,
//! plugin sections, the session line and last the runtime block). Sessions
//! of one profile and model therefore share the first two blocks
//! byte-for-byte. A session builds its prompt once ([`frozen`]); later
//! turns replay the stored bytes.

pub mod blocks;
pub mod context;
pub mod frozen;
pub mod personality;
pub mod platform;
pub mod scan;
pub mod size;

use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::Config;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_providers::request::SystemBlock;
use pmt_store::PromptTier;
use serde::Serialize;

/// A cache tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Same for every session of a profile and model.
    Static,
    /// Same for every session of a profile on one surface.
    Profile,
    /// This session's own text.
    Session,
}

impl Tier {
    /// Tiers in request order.
    pub const ALL: [Self; 3] = [Self::Static, Self::Profile, Self::Session];

    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Profile => "profile",
            Self::Session => "session",
        }
    }
}

/// One block of the prompt, in prompt order (the declaration order is the
/// only placement: docs and the size report read it from here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// `SOUL.md` or the default identity.
    Identity,
    /// How to use the session's tools.
    ToolGuidance,
    /// Advice for the model family.
    ModelGuidance,
    /// The surface's hint.
    PlatformHint,
    /// The skills index.
    SkillsIndex,
    /// Skills loaded into every session.
    PinnedSkills,
    /// The personality overlay.
    Personality,
    /// The caller's own system message.
    CallerSystem,
    /// Project context files.
    ProjectContext,
    /// Memory and user-profile snapshot.
    Memory,
    /// Plugin-provided sections.
    PluginSections,
    /// Session start time and (optionally) id.
    SessionInfo,
    /// The runtime environment block (always last).
    Runtime,
}

impl Layer {
    /// Every layer in prompt order.
    pub const ALL: [Self; 13] = [
        Self::Identity,
        Self::ToolGuidance,
        Self::ModelGuidance,
        Self::PlatformHint,
        Self::SkillsIndex,
        Self::PinnedSkills,
        Self::Personality,
        Self::CallerSystem,
        Self::ProjectContext,
        Self::Memory,
        Self::PluginSections,
        Self::SessionInfo,
        Self::Runtime,
    ];

    /// The tier the layer belongs to.
    #[must_use]
    pub const fn tier(self) -> Tier {
        match self {
            Self::Identity | Self::ToolGuidance | Self::ModelGuidance => Tier::Static,
            Self::PlatformHint | Self::SkillsIndex | Self::PinnedSkills => Tier::Profile,
            _ => Tier::Session,
        }
    }

    /// What the layer holds (the generated system-prompt reference).
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Identity => {
                "The profile's `SOUL.md` when it exists and is not empty, else the built-in identity."
            }
            Self::ToolGuidance => "How to use the tools the session was given.",
            Self::ModelGuidance => "Advice for the model family, when one applies.",
            Self::PlatformHint => {
                "The surface's hint, changed by `prompt.platform_hints.<platform>`."
            }
            Self::SkillsIndex => "The index of available skills.",
            Self::PinnedSkills => "Skills loaded into every session.",
            Self::Personality => "The `agent.personality` overlay.",
            Self::CallerSystem => "The caller's own system message.",
            Self::ProjectContext => "Project context files under `# Project Context`.",
            Self::Memory => "The memory and user-profile snapshot taken at session start.",
            Self::PluginSections => "Sections contributed by plugins.",
            Self::SessionInfo => {
                "The session start time and, with `prompt.include_session_id`, `Session ID: ses_<ULID>`."
            }
            Self::Runtime => "The runtime environment block, always the last text of the prompt.",
        }
    }

    /// The report name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::ToolGuidance => "tool_guidance",
            Self::ModelGuidance => "model_guidance",
            Self::PlatformHint => "platform_hint",
            Self::SkillsIndex => "skills_index",
            Self::PinnedSkills => "pinned_skills",
            Self::Personality => "personality",
            Self::CallerSystem => "caller_system",
            Self::ProjectContext => "project_context",
            Self::Memory => "memory",
            Self::PluginSections => "plugin_sections",
            Self::SessionInfo => "session_info",
            Self::Runtime => "runtime",
        }
    }
}

/// Text between two sections of one tier.
pub const SEPARATOR: &str = "\n\n";

/// One non-empty block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Which block.
    pub layer: Layer,
    /// Its text.
    pub text: String,
}

/// A built system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemPrompt {
    sections: Vec<Section>,
}

impl SystemPrompt {
    /// A prompt of `sections` (kept in layer order).
    #[must_use]
    pub fn new(mut sections: Vec<Section>) -> Self {
        sections.retain(|s| !s.text.is_empty());
        sections.sort_by_key(|s| s.layer);
        Self { sections }
    }

    /// The sections in prompt order.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Each tier's text (sections joined by [`SEPARATOR`]); empty tiers
    /// are left out.
    #[must_use]
    pub fn tiers(&self) -> Vec<PromptTier> {
        Tier::ALL
            .into_iter()
            .filter_map(|tier| {
                let texts: Vec<&str> = self
                    .sections
                    .iter()
                    .filter(|s| s.layer.tier() == tier)
                    .map(|s| s.text.as_str())
                    .collect();
                (!texts.is_empty()).then(|| PromptTier {
                    tier: tier.as_str().to_owned(),
                    text: texts.join(SEPARATOR),
                })
            })
            .collect()
    }
}

/// The blocks of stored tiers, each ending at a cache breakpoint.
#[must_use]
pub fn system_blocks(tiers: &[PromptTier]) -> Vec<SystemBlock> {
    tiers
        .iter()
        .map(|t| SystemBlock {
            text: t.text.clone(),
            cache_breakpoint: true,
        })
        .collect()
}

/// `sha256:<hex>` of the tiers' texts joined by [`SEPARATOR`].
#[must_use]
pub fn hash(tiers: &[PromptTier]) -> String {
    let joined: Vec<&str> = tiers.iter().map(|t| t.text.as_str()).collect();
    crate::sha256(joined.join(SEPARATOR).as_bytes())
}

/// What a session's prompt is built from. Owned, so a build can run on a
/// blocking thread (context files are read under a timeout).
#[derive(Debug, Clone)]
pub struct PromptInputs {
    /// The profile config (`prompt.*`, `agent.personality*`).
    pub config: Config,
    /// The profile directory (holds `SOUL.md`).
    pub profile_dir: PathBuf,
    /// The surface (`cli`, `telegram`...).
    pub platform: String,
    /// An adapter-registered hint replacing the built-in one.
    pub registered_hint: Option<String>,
    /// The working directory (project context, runtime block).
    pub cwd: Option<PathBuf>,
    /// `--no-context-files`.
    pub no_context_files: bool,
    /// The model the prompt is built for.
    pub model: String,
    /// Its context window, when known (context file cap).
    pub context_window: Option<u64>,
    /// The session's exposed tool names.
    pub tool_names: Vec<String>,
    /// The caller's system message.
    pub caller_system: Option<String>,
    /// The skills index (skills subsystem).
    pub skills_index: Option<String>,
    /// Skills pinned into every session (skills subsystem).
    pub pinned_skills: Option<String>,
    /// Memory and user-profile snapshot (memory subsystem).
    pub memory: Option<String>,
    /// Plugin sections.
    pub plugin_sections: Vec<String>,
    /// The session.
    pub session_id: SessionId,
    /// `--pass-session-id`.
    pub pass_session_id: bool,
    /// When the session started.
    pub started_at: Timestamp,
}

/// A built prompt and the problems met on the way (already logged).
#[derive(Debug, Clone)]
pub struct Built {
    /// The prompt.
    pub prompt: SystemPrompt,
    /// Skipped context files and similar warnings.
    pub warnings: Vec<String>,
    /// The project context files loaded, as named in the prompt.
    pub context_files: Vec<String>,
}

/// Builds the prompt of a new session.
///
/// # Errors
///
/// `personality.unknown` when `agent.personality` names no overlay.
pub fn build(inputs: &PromptInputs) -> Result<Built, Error> {
    let config = &inputs.config;
    let mut sections = vec![section(
        Layer::Identity,
        Some(blocks::identity(
            &inputs.profile_dir,
            config.prompt.load_identity,
        )),
    )];
    sections.push(section(
        Layer::ToolGuidance,
        blocks::tool_guidance(&inputs.tool_names),
    ));
    sections.push(section(
        Layer::ModelGuidance,
        blocks::model_guidance(&inputs.model).map(str::to_owned),
    ));
    sections.push(section(
        Layer::PlatformHint,
        platform::hint(
            &inputs.platform,
            inputs.registered_hint.as_deref(),
            &config.prompt.platform_hints,
        ),
    ));
    sections.push(section(Layer::SkillsIndex, inputs.skills_index.clone()));
    sections.push(section(Layer::PinnedSkills, inputs.pinned_skills.clone()));
    let overlay = personality::selected(&config.agent)?;
    sections.push(section(
        Layer::Personality,
        overlay.map(|o| format!("# Personality: {}\n{}", o.name, o.text)),
    ));
    sections.push(section(Layer::CallerSystem, inputs.caller_system.clone()));
    let loaded = project_context(inputs);
    sections.push(section(Layer::ProjectContext, loaded.block));
    sections.push(section(Layer::Memory, inputs.memory.clone()));
    let plugins = inputs.plugin_sections.join(SEPARATOR);
    sections.push(section(Layer::PluginSections, Some(plugins)));
    let shown_id =
        (inputs.pass_session_id || config.prompt.include_session_id).then_some(inputs.session_id);
    sections.push(section(
        Layer::SessionInfo,
        Some(blocks::session_info(inputs.started_at, shown_id)),
    ));
    sections.push(section(
        Layer::Runtime,
        Some(blocks::runtime(&inputs.platform, inputs.cwd.as_deref())),
    ));
    Ok(Built {
        prompt: SystemPrompt::new(sections.into_iter().flatten().collect()),
        warnings: loaded.warnings,
        context_files: loaded.files,
    })
}

fn section(layer: Layer, text: Option<String>) -> Option<Section> {
    text.filter(|t| !t.trim().is_empty())
        .map(|text| Section { layer, text })
}

fn project_context(inputs: &PromptInputs) -> context::Loaded {
    let prompt = &inputs.config.prompt;
    let Some(cwd) = inputs.cwd.as_deref() else {
        return context::Loaded::default();
    };
    if !prompt.load_context_files || inputs.no_context_files {
        return context::Loaded::default();
    }
    context::load(
        cwd,
        context::Limits {
            cap: context::cap(prompt.context_file_max_chars, inputs.context_window),
            read_timeout: Duration::from_secs(prompt.context_file_read_timeout_secs),
        },
    )
}
