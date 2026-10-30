//! Auxiliary task routing (spec: model-providers, Auxiliary task routing;
//! Auxiliary auto route never guesses another account).
//!
//! Side-model calls (titles, compression summaries, vision...) are routed
//! per typed [`AuxTask`]: an explicit call argument wins over
//! `[auxiliary.<task>]`, which wins over `auto`. `auto` is the session's
//! current main route, then the task's `fallback_chain`, then the
//! profile's fallback chain, and otherwise no route at all: the feature
//! degrades with a WARN instead of picking whichever other provider
//! happens to hold a credential. [`AuxCaller`] makes the call with the
//! task's timeout and same-route retries, emitting `aux_call.*` events
//! (never `model.call`) and ledger rows under `auxiliary.<task>`.

mod call;

use std::time::Duration;

use pmt_config::schema::{AuxiliaryConfig, AuxiliaryTask};
use pmt_core::error::Error;
use serde::Serialize;

pub use call::{AuxCaller, AuxEvents, AuxResponse};

use crate::fallback::{self, RouteHealth};
use crate::resolve::{Resolver, Route, Target, Trace};
use crate::target;

macro_rules! tasks {
    ($($variant:ident = $name:literal, $timeout:expr;)*) => {
        /// The auxiliary tasks (`[auxiliary.<task>]` names).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum AuxTask {
            $(#[doc = concat!("`", $name, "`.")] $variant,)*
        }

        impl AuxTask {
            /// Every task, in config order.
            pub const ALL: &[Self] = &[$(Self::$variant),*];

            /// The config name.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// The default timeout; `None` uses the route's request timeout.
            #[must_use]
            pub const fn default_timeout(self) -> Option<Duration> {
                match self {
                    $(Self::$variant => $timeout,)*
                }
            }
        }
    };
}

tasks! {
    Compression = "compression", Some(Duration::from_mins(2));
    Title = "title", Some(Duration::from_secs(30));
    Vision = "vision", Some(Duration::from_mins(2));
    Approval = "approval", Some(Duration::from_secs(30));
    Memory = "memory", None;
    Curator = "curator", Some(Duration::from_mins(10));
    WebExtract = "web_extract", None;
    SkillReview = "skill_review", None;
    Evolution = "evolution", Some(Duration::from_mins(10));
    GoalJudge = "goal_judge", None;
    SideQuestion = "side_question", None;
    VoiceChat = "voice_chat", None;
}

impl AuxTask {
    /// The task named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == name)
    }

    /// The ledger purpose of its calls: `auxiliary.<task>`.
    #[must_use]
    pub fn purpose(self) -> String {
        format!("auxiliary.{}", self.as_str())
    }

    /// The task's `[auxiliary.<task>]` table, when configured.
    #[must_use]
    pub fn settings(self, config: &AuxiliaryConfig) -> Option<&AuxiliaryTask> {
        match self {
            Self::Compression => config.compression.as_ref(),
            Self::Title => config.title.as_ref(),
            Self::Vision => config.vision.as_ref(),
            Self::Approval => config.approval.as_ref(),
            Self::Memory => config.memory.as_ref(),
            Self::Curator => config.curator.as_ref(),
            Self::WebExtract => config.web_extract.as_ref(),
            Self::SkillReview => config.skill_review.as_ref(),
            Self::Evolution => config.evolution.as_ref(),
            Self::GoalJudge => config.goal_judge.as_ref(),
            Self::SideQuestion => config.side_question.as_ref(),
            Self::VoiceChat => config.voice_chat.as_ref(),
        }
    }
}

/// A provider and/or model given by the code making the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuxArgs {
    /// Provider id or alias.
    pub provider: Option<String>,
    /// Model id or alias.
    pub model: Option<String>,
}

/// How an auxiliary route was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AuxSource {
    /// The call's own arguments.
    Explicit,
    /// `[auxiliary.<task>]`.
    Config,
    /// The session's main route (`auto`).
    Main,
    /// Entry `index` of `[auxiliary.<task>] fallback_chain`.
    TaskFallback {
        /// 0-based position.
        index: usize,
    },
    /// Entry `index` of `[[fallback.chain]]`.
    ProfileFallback {
        /// 0-based position.
        index: usize,
    },
}

/// A resolved auxiliary route.
#[derive(Debug, Clone, PartialEq)]
pub struct AuxRoute {
    /// The task.
    pub task: AuxTask,
    /// The route.
    pub route: Route,
    /// Why this route.
    pub source: AuxSource,
    /// Limit of one attempt.
    pub timeout: Duration,
    /// Same-route retries of a retryable failure
    /// (`auxiliary.transient_retries`).
    pub retries: u32,
}

/// The session state auxiliary routing reads.
pub struct AuxContext<'a> {
    /// The profile's resolver (its config holds `[auxiliary]`).
    pub resolver: &'a Resolver<'a>,
    /// The session's current main route; `None` when it is unavailable.
    pub main: Option<&'a Route>,
    /// Benched credentials (a main provider whose every credential is
    /// benched counts as unavailable).
    pub health: &'a dyn RouteHealth,
}

impl std::fmt::Debug for AuxContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxContext").finish_non_exhaustive()
    }
}

/// Resolves the route of `task`.
///
/// `Ok(None)` means the task has no usable route: the caller skips the
/// auxiliary feature (a WARN naming the task is logged here).
///
/// # Errors
///
/// The resolution error of a route pinned by `args` or
/// `[auxiliary.<task>]` (a pinned route never falls back to `auto`).
pub async fn resolve(
    task: AuxTask,
    args: &AuxArgs,
    ctx: &AuxContext<'_>,
) -> Result<Option<AuxRoute>, Error> {
    let config = &ctx.resolver.config.auxiliary;
    let settings = task.settings(config);
    let pinned = pinned(args, settings, ctx)?;
    let found = match pinned {
        Some((target, source)) => {
            let route = ctx
                .resolver
                .resolve_target(&target, &mut Trace::default())
                .await?;
            Some((route, source))
        }
        None => auto(settings, ctx).await,
    };
    let Some((route, source)) = found else {
        tracing::warn!(
            task = task.as_str(),
            "auxiliary task '{}' has no usable route (the main route is unavailable and no fallback answers); skipping it",
            task.as_str()
        );
        return Ok(None);
    };
    let timeout = settings
        .and_then(|s| s.timeout_secs)
        .map(Duration::from_secs)
        .or(task.default_timeout())
        .unwrap_or(route.timeouts.request);
    Ok(Some(AuxRoute {
        task,
        route,
        source,
        timeout,
        retries: config.transient_retries,
    }))
}

/// The target named by the call's arguments, else by config. A model
/// named without a provider runs on the main route's provider (or is an
/// alias); a provider without a model uses that provider's default.
fn pinned(
    args: &AuxArgs,
    settings: Option<&AuxiliaryTask>,
    ctx: &AuxContext<'_>,
) -> Result<Option<(Target, AuxSource)>, Error> {
    let layers = [
        (
            args.provider.clone(),
            args.model.clone(),
            AuxSource::Explicit,
        ),
        (
            settings.and_then(|s| s.provider.clone()),
            settings.and_then(|s| s.model.clone()),
            AuxSource::Config,
        ),
    ];
    for (provider, model, source) in layers {
        let target = match (provider, model) {
            (Some(provider), model) => Target {
                provider,
                model,
                ..Target::default()
            },
            (None, Some(model)) => target::parse(
                &model,
                &ctx.resolver.config.model,
                ctx.resolver.registry,
                ctx.main.map(|r| r.provider.as_str()),
            )?,
            (None, None) => continue,
        };
        return Ok(Some((target, source)));
    }
    Ok(None)
}

/// `auto`: the main route, the task's chain, the profile's chain.
async fn auto(
    settings: Option<&AuxiliaryTask>,
    ctx: &AuxContext<'_>,
) -> Option<(Route, AuxSource)> {
    let now = jiff::Timestamp::now();
    if let Some(main) = ctx.main
        && ctx.health.benched_for(&main.provider, now).is_none()
    {
        return Some((main.clone(), AuxSource::Main));
    }
    let resolver = ctx.resolver;
    let task_chain = settings.and_then(|s| s.fallback_chain.as_ref());
    let current = ctx.main.map(|r| r.provider.as_str());
    let mut chain: Vec<(Target, AuxSource)> = task_chain
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, entry)| {
            let target =
                target::parse(entry, &resolver.config.model, resolver.registry, current).ok()?;
            Some((target, AuxSource::TaskFallback { index }))
        })
        .collect();
    // `fallback_chain = []` disables fallback for the task altogether.
    if task_chain.is_none_or(|entries| !entries.is_empty()) {
        let profile = fallback::chain(resolver.config, resolver.registry).unwrap_or_default();
        chain.extend(
            profile
                .into_iter()
                .enumerate()
                .map(|(index, target)| (target, AuxSource::ProfileFallback { index })),
        );
    }
    for (target, source) in chain {
        let Ok(route) = resolver
            .resolve_target(&target, &mut Trace::default())
            .await
        else {
            continue;
        };
        if ctx.health.benched_for(&route.provider, now).is_none() {
            return Some((route, source));
        }
    }
    None
}

#[cfg(test)]
mod tests;
