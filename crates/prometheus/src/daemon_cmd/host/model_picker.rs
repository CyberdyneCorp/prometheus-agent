//! `model.options` (spec: tui, Discovery RPCs preserve the prompt cache;
//! desktop-app, Composer model picker): every model a session, or a new
//! chat of the profile, may use. That is the current route, the
//! configured default, the model aliases, the Mixture of Agents presets
//! and every model of each connected provider (a provider signed out
//! drops its models until it is signed in again). Each comes with the
//! catalog's context window, the most reasoning it takes and why a
//! switch to it is risky. Nothing here touches the session's prompt.

use pmt_config::schema::{ModelAlias, ReasoningLevel};
use pmt_core::error::Error;
use pmt_rpc::contracts::{ModelOption, ModelOptions};
use pmt_store::Visibility;

use super::{DaemonHost, Profile, session_id};

impl DaemonHost {
    pub(super) async fn models_of(
        &self,
        profile: &Profile,
        session: Option<&str>,
    ) -> Result<ModelOptions, Error> {
        let (current, conversation) = match session {
            Some(session) => Self::session_route(profile, session).await?,
            None => (None, 0),
        };
        let scope = profile.scope();
        let config = scope.config();
        let mut ids: Vec<String> = current.iter().cloned().collect();
        if let (Some(provider), Some(model)) = (&config.model.provider, &config.model.default) {
            ids.push(format!("{provider}/{model}"));
        }
        ids.extend(config.model.aliases.values().filter_map(alias_id));
        ids.extend(crate::model_cmd::picker::preset_ids(config));
        let home = self.launch.user_home().map(std::path::PathBuf::as_path);
        let models = crate::model_cmd::Models::for_config(scope.clone(), config, home)
            .inspect_err(|err| tracing::warn!("model picker without provider catalog: {err}"))
            .ok();
        if let Some(models) = &models {
            for choice in crate::model_cmd::picker::chat_choices(models) {
                ids.extend(choice.models.iter().map(|m| prefixed(&choice.provider, m)));
            }
        }
        let mut options: Vec<ModelOption> = Vec::new();
        for id in ids {
            if options.iter().any(|o| o.id == id) {
                continue;
            }
            let Some((provider, model)) = id.split_once('/') else {
                continue;
            };
            let meta = models
                .as_ref()
                .map(|m| m.catalog.metadata(provider, model).0);
            options.push(option(&id, provider, meta.as_ref(), conversation));
        }
        let current = current.or_else(|| {
            config
                .model
                .default
                .as_ref()
                .zip(config.model.provider.as_ref())
                .map(|(model, provider)| format!("{provider}/{model}"))
        });
        Ok(ModelOptions {
            current,
            models: options,
        })
    }

    /// The loaded session's route and its conversation's estimated tokens.
    async fn session_route(
        profile: &Profile,
        session: &str,
    ) -> Result<(Option<String>, u64), Error> {
        let id = session_id(session)?;
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        let current = match loaded {
            Some(s) => {
                let target = s.live.model.target();
                Some(format!(
                    "{}/{}",
                    target.provider,
                    target.model.unwrap_or_default()
                ))
            }
            None => Self::stored_route(profile, id),
        };
        let bytes: usize = profile
            .store
            .messages(id, Visibility::Active)?
            .iter()
            .filter_map(|m| m.content.as_ref().map(String::len))
            .sum();
        Ok((
            current,
            u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX),
        ))
    }
}

impl DaemonHost {
    /// The route of a session that is not loaded: the one it last ran on,
    /// else the one its client asked for at creation.
    fn stored_route(profile: &Profile, id: pmt_core::id::SessionId) -> Option<String> {
        let row = profile.store.session(id).ok()?;
        if let (Some(provider), Some(model)) = (row.provider, row.model) {
            return Some(format!("{provider}/{model}"));
        }
        let setups = profile.setups.lock().expect("setups poisoned");
        let setup = setups.get(&id)?;
        match (&setup.provider, &setup.model) {
            (Some(provider), Some(model)) if !model.contains('/') => {
                Some(format!("{provider}/{model}"))
            }
            (_, model) => model.clone(),
        }
    }
}

fn alias_id(alias: &ModelAlias) -> Option<String> {
    match alias {
        ModelAlias::Ref(reference) => reference.contains('/').then(|| reference.clone()),
        ModelAlias::Route(route) => Some(format!("{}/{}", route.provider, route.model)),
    }
}

/// `provider/model`, unless the listed id already names its provider.
fn prefixed(provider: &str, model: &str) -> String {
    if model.starts_with(&format!("{provider}/")) {
        model.to_owned()
    } else {
        format!("{provider}/{model}")
    }
}

/// One option; `conversation` is the session's estimated tokens (a
/// conversation larger than the model's window makes the switch risky).
fn option(
    id: &str,
    provider: &str,
    meta: Option<&pmt_providers::metadata::ModelMetadata>,
    conversation: u64,
) -> ModelOption {
    let max_reasoning = meta.map(|meta| {
        pmt_providers::reasoning::clamp(ReasoningLevel::Ultra, meta)
            .0
            .map_or("none", ReasoningLevel::as_str)
            .to_owned()
    });
    let risky = meta.and_then(|meta| {
        if conversation > meta.context_window {
            Some(format!(
                "the conversation (about {conversation} tokens) does not fit its context window ({} tokens)",
                meta.context_window
            ))
        } else if !meta.supports_tools {
            Some("it cannot call tools".to_owned())
        } else {
            None
        }
    });
    ModelOption {
        id: id.to_owned(),
        provider: provider.to_owned(),
        context_window: meta.map(|m| m.context_window),
        max_reasoning,
        risky,
    }
}

#[cfg(test)]
mod tests {
    use super::{option, prefixed};

    #[test]
    fn a_conversation_larger_than_the_window_makes_the_switch_risky() {
        let mut meta = pmt_providers::mock::metadata();
        meta.context_window = 1_000;
        let fits = option("mock/a", "mock", Some(&meta), 900);
        assert_eq!(fits.risky, None);
        let too_big = option("mock/a", "mock", Some(&meta), 5_000);
        assert!(too_big.risky.unwrap().contains("does not fit"));
        meta.supports_tools = false;
        assert_eq!(
            option("mock/a", "mock", Some(&meta), 0).risky.as_deref(),
            Some("it cannot call tools")
        );
    }

    #[test]
    fn listed_ids_get_their_provider_once() {
        assert_eq!(prefixed("openrouter", "a/b"), "openrouter/a/b");
        assert_eq!(prefixed("openrouter", "openrouter/x"), "openrouter/x");
    }
}
