//! Model references (spec: model-providers, User model aliases; /model
//! switch command): what `/model`, `prometheus model set`, `fallback add`
//! and auxiliary fallback slots accept: a `[model.aliases]` name (matched
//! case-insensitively, shadowing every other reading), `<provider>/<model>`
//! with a registered provider name, or a bare model id of the current
//! provider. An alias carries its own provider, endpoint and secret, so a
//! switch never keeps the previous provider's credential.

use pmt_config::schema::{ModelAlias, ModelConfig};
use pmt_core::error::{Error, codes};

use crate::registry::ProviderRegistry;
use crate::resolve::Target;

/// The route of alias `name`, if `[model.aliases]` defines it.
#[must_use]
pub fn alias(config: &ModelConfig, name: &str) -> Option<Target> {
    let (_, entry) = config
        .aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name.trim()))?;
    match entry {
        ModelAlias::Ref(reference) => {
            let (provider, model) = reference.split_once('/')?;
            Some(Target {
                provider: provider.to_owned(),
                model: Some(model.to_owned()),
                ..Target::default()
            })
        }
        ModelAlias::Route(route) => Some(Target {
            provider: route.provider.clone(),
            model: Some(route.model.clone()),
            base_url: route.base_url.clone(),
            credential: route.key_secret.as_ref().map(|s| s.0.clone()),
        }),
    }
}

/// Parses `reference` as an alias, `<provider>/<model>`, or a model of
/// `current` (the session's provider), in that order.
///
/// # Errors
///
/// `provider.unknown` when the part before the first `/` is not a provider
/// name; `model.invalid_ref` for an empty reference, or a bare model id
/// without a current provider.
pub fn parse(
    reference: &str,
    config: &ModelConfig,
    registry: &ProviderRegistry,
    current: Option<&str>,
) -> Result<Target, Error> {
    let reference = reference.trim();
    if let Some(target) = alias(config, reference) {
        return Ok(target);
    }
    match reference.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            let provider = registry.lookup(provider)?;
            Ok(Target {
                provider: provider.id().to_owned(),
                model: Some(model.to_owned()),
                ..Target::default()
            })
        }
        None if !reference.is_empty() => {
            let provider = current.ok_or_else(|| invalid(reference))?;
            Ok(Target {
                provider: provider.to_owned(),
                model: Some(reference.to_owned()),
                ..Target::default()
            })
        }
        _ => Err(invalid(reference)),
    }
}

fn invalid(reference: &str) -> Error {
    Error::new(
        &codes::MODEL_INVALID_REF,
        format!("'{reference}' is not a model: use <provider>/<model> or a [model.aliases] name"),
    )
    .with_hint("prometheus model list")
}

#[cfg(test)]
mod tests {
    use pmt_config::Config;

    use super::*;

    fn setup(text: &str) -> (Config, ProviderRegistry) {
        let config: Config = toml::from_str(text).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        (config, registry)
    }

    #[test]
    fn aliases_match_case_insensitively_and_carry_their_route() {
        let (config, registry) = setup(
            "[model.aliases.fav]\nprovider = \"openrouter\"\nmodel = \"anthropic/claude-x\"\nkey_secret = \"MY_OR_KEY\"\n\
             [model.aliases]\nquick = \"deepseek/deepseek-chat\"\n",
        );
        let fav = parse("FAV", &config.model, &registry, Some("nous")).unwrap();
        assert_eq!(fav.provider, "openrouter");
        assert_eq!(fav.model.as_deref(), Some("anthropic/claude-x"));
        assert_eq!(fav.credential.as_deref(), Some("MY_OR_KEY"));
        let quick = parse("Quick", &config.model, &registry, None).unwrap();
        assert_eq!(quick.provider, "deepseek");
    }

    #[test]
    fn an_alias_shadows_a_provider_reading_of_the_same_text() {
        let (config, registry) = setup("[model.aliases]\n\"or/x\" = \"deepseek/deepseek-chat\"\n");
        let target = parse("or/x", &config.model, &registry, None).unwrap();
        assert_eq!(target.provider, "deepseek");
    }

    #[test]
    fn provider_prefixes_are_canonicalised_and_bare_models_need_a_provider() {
        let (config, registry) = setup("");
        let target = parse("or/openai/gpt-5", &config.model, &registry, None).unwrap();
        assert_eq!(target.provider, "openrouter");
        assert_eq!(target.model.as_deref(), Some("openai/gpt-5"));
        let bare = parse("gpt-5", &config.model, &registry, Some("openai-api")).unwrap();
        assert_eq!(bare.provider, "openai-api");
        let err = parse("gpt-5", &config.model, &registry, None).unwrap_err();
        assert_eq!(err.code().code, "model.invalid_ref");
        assert_eq!(err.exit_code(), 2);
        let err = parse("mistral/large", &config.model, &registry, None).unwrap_err();
        assert_eq!(err.code().code, "provider.unknown");
    }
}
