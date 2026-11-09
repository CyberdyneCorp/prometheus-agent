//! Whole-config checks of the web and media sections the schema cannot
//! express (spec: web-and-media-tools): the xAI search domain filters are
//! mutually exclusive, an image model must be one its built-in provider
//! lists (except open catalogs and custom `openai` endpoints) and a
//! declared command speech provider needs a command.

use pmt_core::error::{Error, codes};

use crate::image_models::{DEFAULT_IMAGE_PROVIDER, image_provider};
use crate::schema::Config;

/// Checks `[web.xai]`, `[image_gen]`, `[tts.providers]` and
/// `[stt.providers]`.
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the offending key.
pub fn check(config: &Config) -> Result<(), Error> {
    let xai = &config.web.xai;
    if !xai.allowed_domains.is_empty() && !xai.excluded_domains.is_empty() {
        return Err(invalid(
            "web.xai.allowed_domains",
            "cannot be combined with web.xai.excluded_domains",
        ));
    }
    check_image_model(config)?;
    for (name, provider) in &config.tts.providers {
        if provider.command.trim().is_empty() {
            return Err(invalid(
                &format!("tts.providers.{name}.command"),
                "is empty",
            ));
        }
    }
    for (name, provider) in &config.stt.providers {
        if provider.command.trim().is_empty() {
            return Err(invalid(
                &format!("stt.providers.{name}.command"),
                "is empty",
            ));
        }
    }
    Ok(())
}

/// The configured image model must be in the catalog of a built-in
/// provider whose catalog is closed; a plugin's provider is not checked.
fn check_image_model(config: &Config) -> Result<(), Error> {
    let image = &config.image_gen;
    let id = image.provider.as_deref().unwrap_or(DEFAULT_IMAGE_PROVIDER);
    let Some(provider) = image_provider(id) else {
        return Ok(());
    };
    let custom_openai =
        id == "openai" && (image.openai.base_url.is_some() || image.openai.provider.is_some());
    if provider.open_catalog || custom_openai {
        return Ok(());
    }
    let (key, model) = match (&image.openai.model, &image.model) {
        (Some(model), _) if id == "openai" => ("image_gen.openai.model", model),
        (_, Some(model)) => ("image_gen.model", model),
        _ => return Ok(()),
    };
    if provider.model(model).is_some() {
        return Ok(());
    }
    Err(invalid(
        key,
        &format!(
            "'{model}' is not a model of image provider '{id}' (valid: {})",
            provider.ids().join(", ")
        ),
    ))
}

fn invalid(key: &str, why: &str) -> Error {
    Error::new(&codes::CONFIG_INVALID, format!("{key}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_image_model_lists_the_valid_ids() {
        let mut config = Config::default();
        config.image_gen.model = Some("dall-e-9".into());
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(err.message().starts_with("image_gen.model: 'dall-e-9'"));
        assert!(err.message().contains("fal-ai/flux-2/klein/9b"));
        // A custom OpenAI-compatible endpoint takes any id.
        config.image_gen.provider = Some("openai".into());
        config.image_gen.model = None;
        config.image_gen.openai.model = Some("grok-imagine-image".into());
        assert!(check(&config).is_err());
        config.image_gen.openai.base_url = Some("http://localhost:18081/v1".into());
        assert!(check(&config).is_ok());
        // OpenRouter's catalog is live.
        config.image_gen.provider = Some("openrouter".into());
        config.image_gen.model = Some("any/model".into());
        assert!(check(&config).is_ok());
    }

    #[test]
    fn xai_domain_filters_are_exclusive() {
        let mut config = Config::default();
        config.web.xai.allowed_domains = vec!["a.test".into()];
        assert!(check(&config).is_ok());
        config.web.xai.excluded_domains = vec!["b.test".into()];
        assert!(
            check(&config)
                .unwrap_err()
                .message()
                .contains("web.xai.allowed_domains")
        );
    }

    #[test]
    fn a_tts_cap_override_must_be_a_positive_integer() {
        use crate::validate::{Node, document};
        for bad in ["0", "-5", "\"long\"", "2.5"] {
            let text = format!("[tts.openai]\nmax_text_length = {bad}\n");
            let doc: toml_edit::Document<String> = text.parse().unwrap();
            let issues = document(&Node::from_item(doc.as_item()));
            assert!(
                issues
                    .iter()
                    .any(|i| i.key == "tts.openai.max_text_length"
                        && i.code == &codes::CONFIG_INVALID),
                "{bad}: {issues:?}"
            );
        }
        let doc: toml_edit::Document<String> =
            "[tts.openai]\nmax_text_length = 9000\n".parse().unwrap();
        assert!(document(&Node::from_item(doc.as_item())).is_empty());
    }
}
