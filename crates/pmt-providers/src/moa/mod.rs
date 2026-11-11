//! Mixture of Agents (spec: mixture-of-agents): the virtual provider
//! `moa` whose models are the presets of `[moa.presets]`. A preset route
//! resolves like any other (so it is selected, cached, billed and used as
//! a fallback entry through the ordinary machinery); sending on it runs
//! the reference models concurrently without tools, then the aggregator
//! with the session's own prompt, tools and history plus the references'
//! guidance ([`call`]). The aggregator's response is the model response.
//!
//! [`Mixture`] holds what persists between a session's calls: the
//! iteration of the current turn and its guidance (the advisor
//! [`cadence`]), the destinations that refused consecutive user messages,
//! the profile's known secrets and where traces go.

pub mod cadence;
mod call;
pub mod guidance;
pub mod privacy;
pub mod trace;
pub mod view;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use pmt_config::Config;
use pmt_config::schema::{MoaConfig, MoaPreset, MoaSlot};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;

pub use cadence::Fanout;

use crate::cost::CallContext;
use crate::registry::ProviderRegistry;
use crate::resolve::{BoxFuture, Resolver, Route, Target, Trace};

/// The provider id.
pub const PROVIDER_ID: &str = "moa";
/// The endpoint every preset route reports (nothing is sent there).
pub const BASE_URL: &str = "moa://local";
/// The name pickers show for the provider.
pub const DISPLAY_NAME: &str = "Mixture of Agents";
/// The ledger purpose of a reference call.
pub const REFERENCE_PURPOSE: &str = "moa.reference";
/// The ledger purpose of an aggregator call.
pub const AGGREGATOR_PURPOSE: &str = "moa.aggregator";

/// Sessions whose state is kept; the least recently used goes first.
const KEPT_SESSIONS: usize = 256;

/// Whether `route` is a MoA preset route.
#[must_use]
pub fn serves(route: &Route) -> bool {
    route.provider == PROVIDER_ID
}

/// The target of a preset slot.
#[must_use]
pub fn slot_target(slot: &MoaSlot) -> Target {
    Target {
        provider: slot.provider.clone(),
        model: Some(slot.model.clone()),
        ..Target::default()
    }
}

/// The route of preset `target.model` (the default preset when unset):
/// the aggregator resolved with its own credential stands for it.
///
/// # Errors
///
/// `model.unknown` listing the presets; the aggregator's resolution
/// error (`provider.missing_credential`...) naming the preset.
pub(crate) fn route<'a>(
    resolver: &'a Resolver<'a>,
    target: &'a Target,
    trace: &'a mut Trace,
) -> BoxFuture<'a, Result<Route, Error>> {
    // Boxed: resolving the aggregator recurses into the resolver.
    Box::pin(async move {
        let moa = &resolver.config.moa;
        let name = target
            .model
            .clone()
            .unwrap_or_else(|| moa.default_preset.clone());
        let preset = trace.check(
            "model",
            moa.preset(&name).ok_or_else(|| unknown(&name, moa)),
        )?;
        trace.ok("model", format!("{DISPLAY_NAME} preset '{name}'"));
        let label = preset.aggregator.label();
        let aggregator = resolver
            .resolve_target(&slot_target(&preset.aggregator), &mut Trace::default())
            .await
            .map_err(|err| {
                let copy = Error::new(
                    err.code(),
                    format!("moa preset '{name}': aggregator {label}: {}", err.message()),
                );
                match err.hint() {
                    Some(hint) => copy.with_hint(hint),
                    None => copy,
                }
            });
        let aggregator = trace.check("credential", aggregator)?;
        trace.ok(
            "credential",
            format!("aggregator {label}: {}", aggregator.credential.describe()),
        );
        let mut route = Route {
            provider: PROVIDER_ID.to_owned(),
            model: name,
            base_url: BASE_URL.to_owned(),
            retry: crate::classify::RetryPolicy::new(0),
            affinity_header: None,
            body_extra: None,
            fast_tier: None,
            key_header: None,
            region: None,
            ..aggregator.clone()
        };
        route.aggregator = Some(Box::new(aggregator));
        Ok(route)
    })
}

/// `model.unknown` for preset `name`.
fn unknown(name: &str, moa: &MoaConfig) -> Error {
    Error::new(
        &codes::MODEL_UNKNOWN,
        format!(
            "'{name}' is not a {DISPLAY_NAME} preset (presets: {})",
            moa.names().join(", ")
        ),
    )
    .with_hint("prometheus moa list")
}

/// Whether `name` is a preset of `config`.
#[must_use]
pub fn has_preset(config: &Config, name: &str) -> bool {
    config.moa.preset(name).is_some()
}

/// Refuses presets whose slots name no registered provider (spec:
/// mixture-of-agents, Preset validation).
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the slot's key.
pub fn validate(config: &Config, registry: &ProviderRegistry) -> Result<(), Error> {
    for (name, preset) in config.moa.all() {
        let slots = preset
            .reference_models
            .iter()
            .enumerate()
            .map(|(i, slot)| (format!("reference_models[{i}]"), slot))
            .chain(std::iter::once((
                "aggregator".to_owned(),
                &preset.aggregator,
            )));
        for (key, slot) in slots {
            if let Err(err) = registry.lookup(&slot.provider) {
                return Err(Error::new(
                    &codes::CONFIG_INVALID,
                    format!("moa.presets.{name}.{key}.provider: {}", err.message()),
                ));
            }
        }
    }
    Ok(())
}

/// `Aggregator is on <provider>; ...` when preset `name`'s aggregator
/// bills a provider other than `model.provider` (spec: mixture-of-agents,
/// MoA billing and accounting).
#[must_use]
pub fn billing_notice(config: &Config, name: &str) -> Option<String> {
    billing_notice_in(
        &*pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        config,
        name,
    )
}

/// [`billing_notice`] worded with `text` (a chat's `/model` switch).
#[must_use]
pub fn billing_notice_in(
    text: &dyn pmt_core::text::Catalog,
    config: &Config,
    name: &str,
) -> Option<String> {
    let preset = config.moa.preset(name)?;
    let main = config.model.provider.as_deref()?;
    let aggregator = &preset.aggregator.provider;
    (aggregator != main).then(|| {
        text.text(
            "slash.model.moa_billing",
            &[("aggregator", aggregator), ("main", &main)],
        )
    })
}

/// One session's MoA state.
#[derive(Debug, Default)]
struct SessionState {
    /// The turn the iteration counts belong to.
    turn: Option<String>,
    /// Model iterations of that turn so far.
    iteration: u32,
    /// The turn's most recent guidance.
    guidance: Option<String>,
    /// Aggregator destinations (base URL, model) that need the guidance
    /// merged into the user message.
    merge: HashSet<(String, String)>,
    /// Use counter (eviction order).
    used: u64,
}

/// Where a call stands in its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    /// The iteration within the turn (1-based).
    pub(crate) iteration: u32,
    /// The turn's guidance so far, when the cadence lets it be reused.
    pub(crate) reuse: Option<String>,
}

/// A profile's MoA state, shared by its sessions (keyed by session).
#[derive(Debug, Default)]
pub struct Mixture {
    profile_dir: Option<PathBuf>,
    secrets: Redactor,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
    clock: Mutex<u64>,
    warned: Mutex<HashSet<String>>,
}

impl Mixture {
    /// The state of the profile at `profile_dir` (traces go under it),
    /// redacting `secrets` from what users see and traces keep.
    #[must_use]
    pub fn new(profile_dir: Option<PathBuf>, secrets: Redactor) -> Self {
        Self {
            profile_dir,
            secrets,
            ..Self::default()
        }
    }

    /// The profile's known secrets.
    #[must_use]
    pub fn secrets(&self) -> &Redactor {
        &self.secrets
    }

    /// The cadence of preset `name`: a malformed `fanout` warns once
    /// naming the preset and behaves as `user_turn`.
    #[must_use]
    pub fn fanout(&self, name: &str, preset: &MoaPreset) -> Fanout {
        Fanout::parse(&preset.fanout).unwrap_or_else(|bad| {
            let first = self
                .warned
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name.to_owned());
            if first {
                tracing::warn!(
                    preset = name,
                    "moa.presets.{name}.fanout {bad} is not user_turn, per_iteration or every_n:N (N >= 2); using user_turn"
                );
            }
            Fanout::UserTurn
        })
    }

    /// Counts the next model iteration of `ctx`'s turn: a new turn starts
    /// at 1 with no guidance.
    pub(crate) fn begin(&self, ctx: Option<&CallContext>, fanout: Fanout) -> Step {
        let Some(ctx) = ctx else {
            return Step {
                iteration: 1,
                reuse: None,
            };
        };
        self.with(ctx.session_id, |state| {
            if state.turn != ctx.turn_id || ctx.turn_id.is_none() {
                state.turn.clone_from(&ctx.turn_id);
                state.iteration = 0;
                state.guidance = None;
            }
            state.iteration += 1;
            let reuse = state
                .guidance
                .clone()
                .filter(|_| !fanout.runs(state.iteration));
            Step {
                iteration: state.iteration,
                reuse,
            }
        })
    }

    /// Keeps `guidance` for the rest of `ctx`'s turn.
    pub(crate) fn keep(&self, ctx: Option<&CallContext>, guidance: &str) {
        if let Some(ctx) = ctx {
            self.with(ctx.session_id, |state| {
                state.guidance = Some(guidance.to_owned());
            });
        }
    }

    /// Whether `route` refused consecutive user messages in this session.
    pub(crate) fn merge_required(&self, ctx: Option<&CallContext>, route: &Route) -> bool {
        ctx.is_some_and(|ctx| {
            self.with(ctx.session_id, |state| {
                state.merge.contains(&destination(route))
            })
        })
    }

    /// Remembers that `route` needs the guidance merged, for the session.
    pub(crate) fn require_merge(&self, ctx: Option<&CallContext>, route: &Route) {
        if let Some(ctx) = ctx {
            self.with(ctx.session_id, |state| {
                state.merge.insert(destination(route));
            });
        }
    }

    fn with<T>(&self, session: SessionId, f: impl FnOnce(&mut SessionState) -> T) -> T {
        let used = {
            let mut clock = self.clock.lock().unwrap_or_else(PoisonError::into_inner);
            *clock += 1;
            *clock
        };
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if !sessions.contains_key(&session) && sessions.len() >= KEPT_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        let state = sessions.entry(session).or_default();
        state.used = used;
        f(state)
    }
}

fn destination(route: &Route) -> (String, String) {
    (
        route.base_url.trim_end_matches('/').to_owned(),
        route.model.clone(),
    )
}
