//! Reasoning effort (spec: model-providers, Reasoning effort defaults,
//! overrides and display).
//!
//! The requested level is the session's `/reasoning` level, else the
//! `[agent.reasoning_overrides]` entry matching the route's model (keys
//! and model ids compared case-insensitively, ignoring `-`, `_`, `.` and
//! any `provider/` prefix; values are levels or `[agent.reasoning_tiers]`
//! names), else `agent.reasoning_effort`, else `medium`. The level sent is
//! clamped down (never up) to the most the model's metadata supports, and
//! no reasoning control is sent to a model that does not reason. A change
//! applies from the next request; the system prompt never changes.

use pmt_config::schema::{AgentConfig, ReasoningLevel};
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::metadata::ModelMetadata;

/// Highest level assumed for a reasoning model whose metadata names none.
pub const ASSUMED_MAX: ReasoningLevel = ReasoningLevel::High;

/// Where the requested level came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EffortSource {
    /// `/reasoning <level>` in this session.
    Session,
    /// `[agent.reasoning_overrides]` entry `key`.
    Override {
        /// The matching key.
        key: String,
    },
    /// `agent.reasoning_effort`.
    Config,
    /// Nothing set: `medium`.
    Default,
}

/// The effort of the next request on a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effort {
    /// The level asked for.
    pub requested: ReasoningLevel,
    /// Where it came from.
    pub source: EffortSource,
    /// The level sent (`None`: no reasoning control is sent).
    pub sent: Option<ReasoningLevel>,
    /// The level was lowered to the model's maximum.
    pub clamped: bool,
}

impl Effort {
    /// How the status line and `model info` show it: `high`,
    /// `high (clamped from ultra)` or `off (model does not reason)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.sent {
            None => "off (model does not reason)".to_owned(),
            Some(level) if self.clamped => {
                format!(
                    "{} (clamped from {})",
                    level.as_str(),
                    self.requested.as_str()
                )
            }
            Some(level) => level.as_str().to_owned(),
        }
    }
}

/// The effort for `model` (the route's model id) with metadata `meta`.
///
/// # Errors
///
/// `reasoning.invalid` when the matching override names neither a level
/// nor a tier.
pub fn effort(
    agent: &AgentConfig,
    session: Option<ReasoningLevel>,
    model: &str,
    meta: &ModelMetadata,
) -> Result<Effort, Error> {
    let (requested, source) = requested(agent, session, model)?;
    let (sent, clamped) = clamp(requested, meta);
    Ok(Effort {
        requested,
        source,
        sent,
        clamped,
    })
}

/// The requested level and its source, before clamping.
///
/// # Errors
///
/// As [`effort`].
pub fn requested(
    agent: &AgentConfig,
    session: Option<ReasoningLevel>,
    model: &str,
) -> Result<(ReasoningLevel, EffortSource), Error> {
    if let Some(level) = session {
        return Ok((level, EffortSource::Session));
    }
    let wanted = normalise(model);
    if let Some((key, value)) = agent
        .reasoning_overrides
        .iter()
        .find(|(key, _)| normalise(key) == wanted)
    {
        let level = level_of(agent, value)
            .ok_or_else(|| invalid(&format!("agent.reasoning_overrides.\"{key}\""), value))?;
        let source = EffortSource::Override { key: key.clone() };
        return Ok((level, source));
    }
    Ok(agent
        .reasoning_effort
        .map_or((ReasoningLevel::Medium, EffortSource::Default), |l| {
            (l, EffortSource::Config)
        }))
}

/// `requested` lowered to what `meta` supports: `None` for a model that
/// does not reason, else at most its maximum (`ASSUMED_MAX` when unknown).
#[must_use]
pub fn clamp(requested: ReasoningLevel, meta: &ModelMetadata) -> (Option<ReasoningLevel>, bool) {
    if !meta.supports_reasoning {
        return (None, false);
    }
    let max = meta.max_reasoning_effort.unwrap_or(ASSUMED_MAX);
    if requested > max {
        (Some(max), true)
    } else {
        (Some(requested), false)
    }
}

/// Checks every override value and tier target (`config validate`).
///
/// # Errors
///
/// `reasoning.invalid` naming the first bad key.
pub fn check(agent: &AgentConfig) -> Result<(), Error> {
    for (tier, level) in &agent.reasoning_tiers {
        if ReasoningLevel::parse(level).is_none() {
            return Err(invalid(&format!("agent.reasoning_tiers.{tier}"), level));
        }
    }
    for (key, value) in &agent.reasoning_overrides {
        if level_of(agent, value).is_none() {
            return Err(invalid(
                &format!("agent.reasoning_overrides.\"{key}\""),
                value,
            ));
        }
    }
    Ok(())
}

/// A tier name (mapped through `[agent.reasoning_tiers]`) or a level.
fn level_of(agent: &AgentConfig, value: &str) -> Option<ReasoningLevel> {
    agent
        .reasoning_tiers
        .get(value)
        .and_then(|level| ReasoningLevel::parse(level))
        .or_else(|| ReasoningLevel::parse(value))
}

/// A model id for matching: without `provider/` prefixes, lower-cased,
/// without `-`, `_` and `.`.
fn normalise(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn invalid(key: &str, value: &str) -> Error {
    let levels: Vec<&str> = ReasoningLevel::ALL.iter().map(|l| l.as_str()).collect();
    Error::new(
        &codes::REASONING_INVALID,
        format!(
            "{key}: '{value}' is neither a reasoning level ({}) nor a tier of [agent.reasoning_tiers]",
            levels.join(", ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(text: &str) -> AgentConfig {
        let config: pmt_config::Config = toml::from_str(text).unwrap();
        config.agent
    }

    fn reasoning_model(max: Option<ReasoningLevel>) -> ModelMetadata {
        ModelMetadata {
            supports_reasoning: true,
            max_reasoning_effort: max,
            ..ModelMetadata::default()
        }
    }

    #[test]
    fn override_matched_despite_spelling() {
        let agent = agent("[agent.reasoning_overrides]\n\"gpt-5.5\" = \"high\"\n");
        let meta = reasoning_model(Some(ReasoningLevel::Xhigh));
        let effort = effort(&agent, None, "openai/gpt_5_5", &meta).unwrap();
        assert_eq!(effort.sent, Some(ReasoningLevel::High));
        assert_eq!(
            effort.source,
            EffortSource::Override {
                key: "gpt-5.5".to_owned()
            }
        );
        for spelling in ["GPT-5.5", "gpt55", "openrouter/openai/gpt.5.5"] {
            assert_eq!(
                requested(&agent, None, spelling).unwrap().0,
                ReasoningLevel::High
            );
        }
        assert_eq!(
            requested(&agent, None, "gpt-5.4").unwrap(),
            (ReasoningLevel::Medium, EffortSource::Default)
        );
    }

    #[test]
    fn clamp_downward_never_upward() {
        let agent = agent("");
        let meta = reasoning_model(Some(ReasoningLevel::High));
        let ultra = effort(&agent, Some(ReasoningLevel::Ultra), "m", &meta).unwrap();
        assert_eq!(ultra.sent, Some(ReasoningLevel::High));
        assert!(ultra.clamped);
        assert_eq!(ultra.describe(), "high (clamped from ultra)");
        let low = effort(&agent, Some(ReasoningLevel::Low), "m", &meta).unwrap();
        assert_eq!((low.sent, low.clamped), (Some(ReasoningLevel::Low), false));
        assert_eq!(low.describe(), "low");
    }

    #[test]
    fn no_reasoning_controls_for_a_model_without_reasoning() {
        let effort = effort(
            &agent(""),
            Some(ReasoningLevel::High),
            "m",
            &ModelMetadata::default(),
        )
        .unwrap();
        assert_eq!(effort.sent, None);
        assert_eq!(effort.describe(), "off (model does not reason)");
    }

    #[test]
    fn precedence_and_tiers() {
        let agent = agent(
            "[agent]\nreasoning_effort = \"low\"\n\
             [agent.reasoning_tiers]\nultra = \"max\"\n\
             [agent.reasoning_overrides]\nopus = \"ultra\"\n",
        );
        assert_eq!(
            requested(&agent, None, "anthropic/opus").unwrap().0,
            ReasoningLevel::Max
        );
        assert_eq!(
            requested(&agent, None, "other").unwrap(),
            (ReasoningLevel::Low, EffortSource::Config)
        );
        assert_eq!(
            requested(&agent, Some(ReasoningLevel::Minimal), "opus")
                .unwrap()
                .1,
            EffortSource::Session
        );
        check(&agent).unwrap();
        let bad = self::agent("[agent.reasoning_overrides]\nx = \"hgih\"\n");
        let err = check(&bad).unwrap_err();
        assert_eq!(err.code().code, "reasoning.invalid");
        assert!(err.message().contains("agent.reasoning_overrides.\"x\""));
    }
}
