//! Voice turn model and reasoning (spec: voice-mode, Voice turn model and
//! reasoning). A chained voice turn (a spoken CLI, TUI or desktop message,
//! a gateway voice note) runs on `[auxiliary.voice_chat]` when it names a
//! provider or model (unset is `auto`: the session's own route) with
//! reasoning at `auxiliary.voice_chat.reasoning_effort`, `none` by default
//! (each wire maps `none` to the lowest effort the model accepts). The
//! switch is a `--once` route with the prompt kept, so the session's
//! toolset and cached prefix stay as they are and the next typed message
//! is back on the session's route. A conversation longer than the voice
//! model's window stays on the session's route, with a notice the first
//! time.

use std::sync::Arc;

use pmt_config::schema::ReasoningLevel;
use pmt_core::error::Error;
use pmt_providers::resolve::Target;
use pmt_store::Store;

use super::Live;
use super::model::Models;

/// What a voice turn changed, put back by [`VoiceTurn::end`].
pub struct VoiceTurn {
    /// The route was switched for this turn.
    switched: bool,
    /// The reasoning level before the turn.
    reasoning: Option<ReasoningLevel>,
    /// Shown once per session: the voice model's window is too small.
    pub notice: Option<String>,
}

/// The store preference marking that `session` was told its voice turns
/// stay on the session's route.
fn noticed_pref(session: pmt_core::id::SessionId) -> String {
    format!("voice.window_notice.{session}")
}

/// The tokens the session's next request starts from: its last model
/// call's input.
fn context_tokens(store: &Store, session: pmt_core::id::SessionId) -> u64 {
    store
        .usage_events(session)
        .ok()
        .and_then(|events| {
            events
                .iter()
                .rev()
                .find(|e| e.purpose == "main")
                .map(|e| e.tokens.input + e.tokens.cache_read + e.tokens.cache_write)
        })
        .unwrap_or(0)
}

/// Puts `live` on its voice route and reasoning for the turn about to
/// run.
///
/// # Errors
///
/// A configured voice route that does not resolve.
pub async fn begin(models: &Models, store: &Store, live: &Live) -> Result<VoiceTurn, Error> {
    let config = live.scope.config();
    let task = config.auxiliary.voice_chat.as_ref();
    let reasoning = reasoning_of(task);
    let mut turn = VoiceTurn {
        switched: false,
        reasoning: live.model.reasoning_now(),
        notice: None,
    };
    let resolver = models.resolver();
    if let Some(target) = task
        .filter(|t| t.provider.is_some() || t.model.is_some())
        .map(|t| voice_target(t, &resolver, &live.model.target()))
        .transpose()?
    {
        let route = resolver
            .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
            .await?;
        let window = models
            .catalog()
            .metadata(&route.provider, &route.model)
            .0
            .context_window;
        if window > 0 && context_tokens(store, live.id) > window {
            turn.notice = window_notice(store, live, &route.model);
        } else {
            let target = super::model::target_of(&route);
            let port = models.port(&target).await?;
            live.agent.keep_prompt(true);
            live.model.switch_once(target, Arc::clone(&port));
            turn.switched = true;
        }
    }
    live.model.set_reasoning(Some(reasoning));
    Ok(turn)
}

/// A voice turn's reasoning: `auxiliary.voice_chat.reasoning_effort`,
/// `none` by default.
fn reasoning_of(task: Option<&pmt_config::schema::AuxiliaryTask>) -> ReasoningLevel {
    task.and_then(|t| t.reasoning_effort)
        .unwrap_or(ReasoningLevel::None)
}

/// `[auxiliary.voice_chat]`'s route: its provider and model, or a model
/// reference (an alias, `<provider>/<model>`, or a model of the session's
/// provider) as `/model` reads it.
fn voice_target(
    task: &pmt_config::schema::AuxiliaryTask,
    resolver: &pmt_providers::resolve::Resolver<'_>,
    session: &Target,
) -> Result<Target, Error> {
    match (&task.provider, &task.model) {
        (Some(provider), model) => Ok(Target {
            provider: provider.clone(),
            model: model.clone(),
            ..Target::default()
        }),
        (None, Some(model)) => pmt_providers::target::parse(
            model,
            &resolver.config.model,
            resolver.registry,
            Some(&session.provider),
        ),
        (None, None) => Ok(session.clone()),
    }
}

/// The one-time notice that voice turns stay on the session's route.
fn window_notice(store: &Store, live: &Live, model: &str) -> Option<String> {
    let pref = noticed_pref(live.id);
    if store.pref(&pref).ok().flatten().is_some() {
        return None;
    }
    let _ = store.set_pref(&pref, "1");
    Some(format!(
        "This conversation is longer than {model}'s context window, so voice turns use the session's model."
    ))
}

impl VoiceTurn {
    /// Puts back the session's route and reasoning once the turn ended.
    pub fn end(self, live: &Live) {
        if self.switched {
            live.model.end_turn();
            live.agent.keep_prompt(false);
        }
        live.model.set_reasoning(self.reasoning);
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::AuxiliaryTask;

    use super::*;

    #[test]
    fn voice_turns_reason_at_none_unless_configured() {
        assert_eq!(reasoning_of(None), ReasoningLevel::None);
        let routed = AuxiliaryTask {
            model: Some("fast".to_owned()),
            ..AuxiliaryTask::default()
        };
        assert_eq!(reasoning_of(Some(&routed)), ReasoningLevel::None);
        let low = AuxiliaryTask {
            reasoning_effort: Some(ReasoningLevel::Low),
            ..AuxiliaryTask::default()
        };
        assert_eq!(reasoning_of(Some(&low)), ReasoningLevel::Low);
    }
}
