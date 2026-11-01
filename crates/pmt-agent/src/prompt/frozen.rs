//! The prompt a session froze (spec: agent-turn-loop, System prompt frozen
//! per session).
//!
//! A session's first turn builds the prompt and stores its bytes, hash and
//! tool schemas; every later turn replays them, so a resumed session sends
//! what it sent before even when `SOUL.md`, memory, skills or the
//! personality changed on disk. A rebuild happens only for a new session,
//! a missing stored prompt, a model or provider change, a committed
//! compaction or an explicit `--now`, and each one names its reason (the
//! caller emits `prompt.rebuilt`).

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_providers::request::Tool;
use pmt_store::{PromptTier, SessionPrompt, Store, Visibility};

use super::Built;

/// Why a prompt is (re)built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuild {
    /// The session's first turn.
    NewSession,
    /// The session has turns but no stored prompt (older store, deleted
    /// row).
    Missing,
    /// The route's provider or model differs from the stored one.
    ModelChanged,
    /// A compaction committed.
    Compaction,
    /// The user applied a deferred change now (`personality --now`...).
    Now(String),
}

impl Rebuild {
    /// The `prompt.rebuilt` reason.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::NewSession => "new_session".to_owned(),
            Self::Missing => "missing".to_owned(),
            Self::ModelChanged => "model_changed".to_owned(),
            Self::Compaction => "compaction".to_owned(),
            Self::Now(what) => format!("{what} --now"),
        }
    }
}

/// The prompt and tools every request of the session carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Frozen {
    /// The tiers, one system block each.
    pub tiers: Vec<PromptTier>,
    /// `sha256:<hex>` of the prompt text.
    pub sha256: String,
    /// The tool schemas in request order.
    pub tools: Vec<Tool>,
    /// Provider it was built for.
    pub provider: Option<String>,
    /// Model it was built for.
    pub model: Option<String>,
}

impl Frozen {
    fn of(stored: SessionPrompt) -> Result<Self, Error> {
        let tools = serde_json::from_value(stored.tools).map_err(|e| {
            Error::new(
                &codes::STORE_SQLITE,
                format!("state.db: session_prompts.tools does not parse: {e}"),
            )
        })?;
        Ok(Self {
            tiers: stored.tiers,
            sha256: stored.sha256,
            tools,
            provider: stored.provider,
            model: stored.model,
        })
    }
}

/// Whether `session`'s stored prompt can be replayed on `provider`/`model`;
/// `Err(rebuild)` says why it cannot.
///
/// # Errors
///
/// Store errors.
pub fn stored(
    store: &Store,
    session: SessionId,
    provider: &str,
    model: &str,
) -> Result<Result<Frozen, Rebuild>, Error> {
    let Some(prompt) = store.session_prompt(session)? else {
        let has_turns = store
            .messages(session, Visibility::Active)?
            .iter()
            .any(|m| m.role == pmt_store::Role::Assistant);
        return Ok(Err(if has_turns {
            Rebuild::Missing
        } else {
            Rebuild::NewSession
        }));
    };
    if prompt.provider.as_deref() != Some(provider) || prompt.model.as_deref() != Some(model) {
        return Ok(Err(Rebuild::ModelChanged));
    }
    Frozen::of(prompt).map(Ok)
}

/// Stores a freshly built prompt for `session`.
///
/// # Errors
///
/// Store errors.
pub fn save(
    store: &Store,
    session: SessionId,
    built: &Built,
    tools: Vec<Tool>,
    route: (&str, &str),
    why: &Rebuild,
) -> Result<Frozen, Error> {
    let tiers = built.prompt.tiers();
    let sha256 = super::hash(&tiers);
    let record = SessionPrompt {
        tiers: tiers.clone(),
        sha256: sha256.clone(),
        tools: serde_json::to_value(&tools).unwrap_or_default(),
        provider: Some(route.0.to_owned()),
        model: Some(route.1.to_owned()),
        built_at: Timestamp::now(),
        reason: why.reason(),
    };
    store.save_session_prompt(session, &record)?;
    Ok(Frozen {
        tiers,
        sha256,
        tools,
        provider: record.provider,
        model: record.model,
    })
}
