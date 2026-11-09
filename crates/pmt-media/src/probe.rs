//! Availability of the media tools (spec: tool-system, Availability probes
//! are cached per profile with flake grace; web-and-media-tools, Media
//! tool credentials and billing are explicit): a tool whose configured
//! provider has no credential is left out of the surface with the reason,
//! never offered to fail at every call. Probes see only the profile's
//! config and which credentials exist (`signin:<provider>` for sign-ins):
//! a name that is not built in may be a provider an enabled plugin
//! registered, which only the session knows, so the tool is offered and a
//! call fails closed (`<category> provider '<name>' is not installed or
//! not enabled`) when nothing provides it, as the web tools do.

use pmt_config::Config;
use pmt_config::image_models::image_provider;
use pmt_tools::probe::{ProbeContext, signin};

type Presence<'a> = &'a (dyn Fn(&str) -> bool + Send + Sync);

fn needs(has: Presence<'_>, names: &[String], what: &str) -> Result<(), String> {
    if names.iter().any(|n| has(n)) {
        Ok(())
    } else {
        Err(format!("needs {what}"))
    }
}

/// `image_generate`: the selected built-in provider's credential, or,
/// unset, any detectable one (`FAL_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`,
/// a Nous sign-in).
///
/// # Errors
///
/// The missing credential.
pub fn image(context: ProbeContext<'_>) -> Result<(), String> {
    let config: &Config = context.config;
    let has = context.has_secret;
    let image = &config.image_gen;
    match image.provider.as_deref() {
        None => needs(
            has,
            &[
                "FAL_KEY".into(),
                "OPENAI_API_KEY".into(),
                "XAI_API_KEY".into(),
                signin("nous"),
            ],
            "FAL_KEY (image_gen.provider unset; fal is the default)",
        ),
        Some("nous") => needs(has, &[signin("nous")], "a Nous Portal sign-in"),
        Some("openai-codex") => needs(has, &[signin("openai-codex")], "an openai-codex sign-in"),
        Some("openai") if image.openai.base_url.is_some() || image.openai.provider.is_some() => {
            Ok(())
        }
        Some("openai") => {
            let secret = image
                .openai
                .key_secret
                .as_ref()
                .map_or("OPENAI_API_KEY", |s| s.0.as_str());
            needs(has, &[secret.to_owned()], secret)
        }
        Some("xai") => needs(
            has,
            &["XAI_API_KEY".into(), signin("xai-oauth")],
            "XAI_API_KEY or an xai-oauth sign-in",
        ),
        Some(other) => match image_provider(other).and_then(|p| p.secret) {
            Some(secret) => needs(has, &[secret.to_owned()], secret),
            None => Ok(()),
        },
    }
}

/// `text_to_speech`: the keyless default, local engines, command and
/// plugin providers are always offered; a selected cloud provider needs
/// its key (or the Nous sign-in).
///
/// # Errors
///
/// The missing credential.
pub fn speech(context: ProbeContext<'_>) -> Result<(), String> {
    let tts = &context.config.tts;
    let Some(provider) = tts.provider.as_deref() else {
        return Ok(());
    };
    if !crate::speech::tts::BUILTIN.contains(&provider) {
        return Ok(());
    }
    if provider == "nous" {
        return needs(
            context.has_secret,
            &[signin("nous")],
            "a Nous Portal sign-in",
        );
    }
    match crate::speech::tts::secret(provider) {
        Some(secret) => needs(context.has_secret, &[secret.to_owned()], secret),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        config: &Config,
        keys: &[&str],
        probe: fn(ProbeContext<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        let has = move |name: &str| keys.iter().any(|k| k == name);
        probe(ProbeContext {
            profile: "default",
            config,
            has_secret: &has,
        })
    }

    #[test]
    fn image_generate_needs_the_selected_providers_credential() {
        let mut config = Config::default();
        assert!(check(&config, &[], image).unwrap_err().contains("FAL_KEY"));
        assert!(check(&config, &["signin:nous"], image).is_ok());
        config.image_gen.provider = Some("openai".into());
        assert_eq!(
            check(&config, &["FAL_KEY"], image),
            Err("needs OPENAI_API_KEY".into())
        );
        config.image_gen.openai.base_url = Some("http://localhost:18081/v1".into());
        assert!(check(&config, &[], image).is_ok());
        // Maybe a plugin's: the call fails closed when nothing provides it.
        config.image_gen.provider = Some("midjourney".into());
        assert!(check(&config, &[], image).is_ok());
    }

    #[test]
    fn speech_is_offered_keyless_by_default_and_needs_a_selected_cloud_key() {
        let mut config = Config::default();
        assert!(check(&config, &[], speech).is_ok());
        config.tts.provider = Some("elevenlabs".into());
        assert_eq!(
            check(&config, &[], speech),
            Err("needs ELEVENLABS_API_KEY".into())
        );
        config.tts.provider = Some("piper".into());
        assert!(check(&config, &[], speech).is_ok());
        config.tts.provider = Some("mine".into());
        assert!(check(&config, &[], speech).is_ok());
    }
}
