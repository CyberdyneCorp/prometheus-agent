//! `prometheus setup model`: the provider and model (or a subscription
//! sign-in), then the provider's API key into the secrets store.

use pmt_config::ProfileScope;
use pmt_config::schema::ProviderAuth;
use pmt_core::error::Error;

use super::Wizard;
use crate::context::Context;
use crate::model_cmd::Models;
use crate::model_cmd::picker::{Picked, pick};

/// Whether the profile names a model or a provider.
pub(super) fn configured(scope: &ProfileScope) -> bool {
    let model = &scope.config().model;
    model.default.is_some() || model.provider.as_deref().is_some_and(|p| p != "auto")
}

/// `provider/model` as configured, for display.
fn current(scope: &ProfileScope) -> String {
    let model = &scope.config().model;
    match (&model.provider, &model.default) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (Some(p), None) => format!("{p} (its default model)"),
        (None, Some(m)) => m.clone(),
        (None, None) => "not configured".to_owned(),
    }
}

/// The interactive model section.
pub(super) fn interactive(ctx: &Context<'_>, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let models = Models::open(ctx)?;
    wizard.say(&format!(
        "Model — current: {} (Enter keeps it)",
        current(&models.scope)
    ))?;
    let picked = pick(&models, &mut wizard.input, &mut wizard.out, true)?;
    match picked {
        None => wizard.say("model unchanged"),
        Some(Picked::SignIn(provider)) => {
            crate::auth_cmd::sign_in_default(ctx, provider)?;
            super::write_changed(&models.scope, &[("model.provider", provider.to_owned())])?;
            wizard.say(&format!("signed in; model.provider = \"{provider}\""))
        }
        Some(Picked::Model(reference)) => {
            let provider = reference.split('/').next().unwrap_or_default();
            ensure_key(&models, provider, wizard)?;
            let unchanged = models.scope.config().model.provider.as_deref() == Some(provider)
                && models.scope.config().model.default.as_deref()
                    == reference.split_once('/').map(|(_, m)| m);
            if unchanged {
                return wizard.say("model unchanged");
            }
            crate::model_cmd::set_model(ctx, &reference).map(drop)
        }
    }
}

/// Asks for `provider`'s API key when it needs one and none resolves.
fn ensure_key(models: &Models, provider: &str, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let provider = models.registry.lookup(provider)?;
    if provider.auth() != ProviderAuth::ApiKey {
        return Ok(());
    }
    let names = provider.secret_names();
    let secrets = models.scope.secrets();
    if names.iter().any(|name| secrets.resolve(name).is_ok()) {
        return Ok(());
    }
    let Some(name) = names.first() else {
        return Ok(());
    };
    match wizard.ask_secret(name)? {
        Some(value) => {
            secrets.store().set(name, &value)?;
            wizard.say(&format!("{name} stored in the secrets store"))
        }
        None => wizard.say(&format!(
            "{name} skipped; set it later: prometheus secrets set {name}"
        )),
    }
}
