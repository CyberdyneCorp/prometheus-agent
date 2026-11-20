//! `/hatch`'s image backend (spec: pets, Generating a pet):
//! `pets.image_provider` when set, else the first configured image
//! provider (the profile's own first, then the built-in catalog in
//! order) whose model takes reference images and whose credential
//! resolves. Nothing is generated while choosing.

use std::sync::Arc;

use base64::Engine as _;
use pmt_config::Config;
use pmt_config::ProfileScope;
use pmt_config::image_models::IMAGE_PROVIDERS;
use pmt_core::error::Error;
use pmt_media::Credentials;
use pmt_media::image::providers::{self, Aspect, Request};
use pmt_media::image::{self, Plan};
use pmt_pets::hatch::{Backend, BackendInfo, Generation};

/// A chosen provider and model.
pub struct ImageBackend {
    plan: Plan,
    client: reqwest::Client,
}

/// The estimated price of one image of `model` (`None`: unknown).
fn price(plan: &Plan) -> Option<f64> {
    let gpt = plan.model.api_model.starts_with("gpt-image") || plan.model.id.contains("gpt-image");
    match (gpt, plan.model.quality.as_deref()) {
        (true, Some("low")) => Some(0.011),
        (true, Some("medium")) => Some(0.042),
        (true, Some("high")) => Some(0.167),
        _ => None,
    }
}

impl Backend for ImageBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            provider: self.plan.provider.clone(),
            model: self.plan.model.id.clone(),
            usd_per_image: price(&self.plan),
        }
    }

    fn generate<'a>(&'a self, prompt: &'a str, reference: Option<&'a [u8]>) -> Generation<'a> {
        Box::pin(async move {
            let references = reference
                .map(|png| {
                    vec![format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(png)
                    )]
                })
                .unwrap_or_default();
            let request = Request {
                prompt: prompt.to_owned(),
                aspect: if reference.is_some() {
                    Aspect::Landscape
                } else {
                    Aspect::Square
                },
                image_url: None,
                references,
                upscale: false,
            };
            providers::generate(&self.client, &self.plan, &request)
                .await
                .map(|generated| generated.bytes)
        })
    }
}

/// The candidates in order: `pets.image_provider`, else the configured
/// image provider and then every built-in one.
fn candidates(config: &Config, wanted: &str) -> Vec<String> {
    if !wanted.is_empty() {
        return vec![wanted.to_owned()];
    }
    let configured = image::configured_provider(config).to_owned();
    let mut ids = vec![configured.clone()];
    ids.extend(
        IMAGE_PROVIDERS
            .iter()
            .map(|p| p.id.to_owned())
            .filter(|id| *id != configured),
    );
    ids
}

/// `config` set up to plan `provider`, with a reference-capable model (the
/// configured one when it is, else the provider's first edit model).
fn for_provider(config: &Config, provider: &str) -> Result<Config, String> {
    let mut config = config.clone();
    let own = image::configured_provider(&config) == provider;
    config.image_gen.provider = Some(provider.to_owned());
    if !own {
        config.image_gen.model = None;
        config.image_gen.openai.model = None;
    }
    let model = image::model_for(&config, provider)
        .ok_or_else(|| format!("image provider '{provider}' is not installed"))?;
    if model.supports_edit && model.max_reference_images > 0 {
        return Ok(config);
    }
    let edit = image::edit_capable(provider)
        .ok_or_else(|| format!("{provider}/{} takes no reference images", model.id))?;
    if own && config.image_gen.model.is_some() {
        return Err(format!("{provider}/{} takes no reference images", model.id));
    }
    config.image_gen.model = Some(edit.to_owned());
    config.image_gen.openai.model = None;
    Ok(config)
}

/// The backend `/hatch` uses.
///
/// # Errors
///
/// `pet.no_image_backend` naming why the first candidate is unfit;
/// network setup errors.
pub fn select(scope: &ProfileScope) -> Result<Arc<dyn Backend>, Error> {
    let base = crate::chat::session::media::credentials(scope);
    let wanted = crate::pets_host::current(scope).image_provider;
    let mut first_reason = None;
    for provider in candidates(scope.config(), &wanted) {
        let planned = for_provider(scope.config(), &provider).and_then(|config| {
            let credentials = Credentials {
                config: Arc::new(config),
                ..base.clone()
            };
            image::plan(&credentials, &|_, default| default.to_owned())
        });
        match planned {
            Ok(plan) if plan.model.supports_edit => {
                let network = &scope.config().network;
                let client = pmt_net::client(&pmt_net::OutboundPolicy {
                    force_ipv4: network.force_ipv4,
                    extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
                    proxy: network.proxy.clone(),
                })?;
                return Ok(Arc::new(ImageBackend { plan, client }));
            }
            Ok(plan) => {
                first_reason.get_or_insert(format!(
                    "{}/{} takes no reference images",
                    plan.provider, plan.model.id
                ));
            }
            Err(why) => {
                first_reason.get_or_insert(why);
            }
        }
    }
    Err(pmt_pets::hatch::no_backend(first_reason.as_deref()))
}
