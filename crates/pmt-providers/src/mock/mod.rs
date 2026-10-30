//! The built-in deterministic `mock` provider (spec: replay-and-evaluation,
//! Deterministic mock provider; Mock fault injection): always available,
//! no credential, no network. Each call is answered in-process by the
//! first unused [`Script`] rule that matches it, or, without a script, by
//! echoing the last user message prefixed with `mock: `.
//!
//! A rule matches on the step index (the number of assistant messages
//! already in the request, so the first call of a conversation is step
//! 0), the route's model, the last user message and the last tool result
//! (substring or regex). A rule answers one call unless it sets
//! `times = N` or `repeat = true`, so a script reads as the conversation
//! it plays. Injected faults go through the same classification as real
//! provider failures, so retries and the fallback chain treat them alike.
//! Given the same script and the same requests, the streams are
//! byte-identical.

mod fault;
mod feed;
mod script;

use std::path::Path;
use std::sync::Mutex;

use pmt_config::Config;
use pmt_core::error::Error;
use pmt_obs::usage::Price;

pub use script::Script;

pub(crate) use feed::Feed;
use script::{Answer, Response, Rule};

use crate::classify::{ErrorClass, ProviderError};
use crate::metadata::{ContextSource, DEFAULT_CONTEXT_WINDOW, ModelMetadata};
use crate::request::{Part, Request, Role};
use crate::resolve::Route;

/// The provider id.
pub const PROVIDER_ID: &str = "mock";

/// The endpoint every mock route reports (nothing is ever sent there).
pub const BASE_URL: &str = "mock://local";

/// The model when none is named.
pub const DEFAULT_MODEL: &str = "mock";

/// Whether `route` is answered by the mock instead of over HTTP.
#[must_use]
pub fn serves(route: &Route) -> bool {
    route.provider == PROVIDER_ID
}

/// What every mock model is: tools, images and reasoning accepted, the
/// default context window (override it with `[models."mock/<m>"]` to
/// exercise compression), free.
#[must_use]
pub fn metadata() -> ModelMetadata {
    ModelMetadata {
        supports_vision: true,
        supports_reasoning: true,
        context_source: ContextSource::Builtin,
        price: Some(Price::default()),
        context_window: DEFAULT_CONTEXT_WINDOW,
        ..ModelMetadata::default()
    }
}

/// A session's mock: its script and how often each rule was used.
#[derive(Debug, Default)]
pub struct Mock {
    script: Option<Script>,
    used: Mutex<Vec<u32>>,
}

impl Mock {
    /// Echo mode: every call answers `mock: <last user message>`.
    #[must_use]
    pub fn echo() -> Self {
        Self::default()
    }

    /// Answers from `script`.
    #[must_use]
    pub fn new(script: Script) -> Self {
        Self {
            used: Mutex::new(vec![0; script.rules.len()]),
            script: Some(script),
        }
    }

    /// The mock of a session: the `--mock-script` file (`flag`, as given)
    /// else `providers.mock.script` (relative to `profile_dir`), else echo
    /// mode.
    ///
    /// # Errors
    ///
    /// `mock.script_missing` or `mock.script_invalid` for the chosen file.
    pub fn for_session(
        flag: Option<&Path>,
        config: &Config,
        profile_dir: &Path,
    ) -> Result<Self, Error> {
        let configured = config
            .providers
            .get(PROVIDER_ID)
            .and_then(|p| p.script.as_deref())
            .map(|path| profile_dir.join(path));
        match flag.map(Path::to_path_buf).or(configured) {
            Some(path) => Script::load(&path).map(Self::new),
            None => Ok(Self::echo()),
        }
    }

    /// The answer to `request` on `route`: a stream to play, or the
    /// injected failure.
    ///
    /// # Errors
    ///
    /// The classified fault of the matching rule, or a `config` class
    /// error when no rule matches.
    pub(crate) fn reply(&self, route: &Route, request: &Request) -> Result<Feed, ProviderError> {
        let call = Call::of(route, request);
        let Some(script) = &self.script else {
            let echo = Response {
                text: Some(format!("mock: {}", call.user.unwrap_or_default())),
                ..Response::default()
            };
            return Ok(Feed::new(
                &echo,
                request,
                call.step,
                std::time::Duration::ZERO,
            ));
        };
        let rule = self.take(script, &call).ok_or_else(|| {
            ProviderError::new(
                ErrorClass::Config,
                format!(
                    "the mock script has no unused rule matching step {} of model '{}'",
                    call.step, call.model
                ),
            )
        })?;
        match &rule.answer {
            Answer::Respond(response) => Ok(Feed::new(response, request, call.step, rule.latency)),
            Answer::Fail(fault) => Err(fault::error(*fault)),
        }
    }

    /// The first unused rule matching `call`, now counted as used.
    fn take<'s>(&self, script: &'s Script, call: &Call<'_>) -> Option<&'s Rule> {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = script
            .rules
            .iter()
            .zip(used.iter())
            .position(|(rule, used)| rule.uses.available(*used) && call.matches(rule))?;
        used[index] += 1;
        Some(&script.rules[index])
    }
}

/// What rules match on.
struct Call<'a> {
    step: usize,
    model: &'a str,
    user: Option<String>,
    tool_result: Option<&'a str>,
}

impl<'a> Call<'a> {
    fn of(route: &'a Route, request: &'a Request) -> Self {
        let messages = &request.messages;
        let step = messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let user = messages
            .iter()
            .rev()
            .filter(|m| m.role == Role::User)
            .find_map(|m| {
                let texts: Vec<&str> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                (!texts.is_empty()).then(|| texts.join("\n"))
            });
        let tool_result = messages.iter().rev().find_map(|m| {
            m.parts.iter().rev().find_map(|p| match p {
                Part::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
        });
        Self {
            step,
            model: &route.model,
            user,
            tool_result,
        }
    }

    fn matches(&self, rule: &Rule) -> bool {
        let m = &rule.matcher;
        m.step.is_none_or(|step| step == self.step)
            && m.model.as_deref().is_none_or(|model| model == self.model)
            && m.user
                .as_ref()
                .is_none_or(|t| self.user.as_deref().is_some_and(|u| t.matches(u)))
            && m.tool_result
                .as_ref()
                .is_none_or(|t| self.tool_result.is_some_and(|r| t.matches(r)))
    }
}

#[cfg(test)]
mod tests;
