//! Image generation (spec: web-and-media-tools, `image_generate` tool;
//! Built-in image generation providers and models).
//!
//! [`plan`] settles the provider, model and credential of a call from
//! `[image_gen]` (the agent never picks the model); [`providers`] speaks
//! each provider's API; [`tool`] is the model-facing tool.

pub mod providers;
pub mod tool;

use pmt_config::Config;
use pmt_config::image_models::{DEFAULT_IMAGE_PROVIDER, ImageModel, image_provider};
use pmt_providers::auth::gateway::Category;

use crate::access::{Access, Credentials};

/// Most generations of one batch at once (`image_gen.max_parallel_requests`
/// within 1..`agent.tool_concurrency`).
#[must_use]
pub fn parallel_limit(config: &Config) -> usize {
    let most = config.agent.tool_concurrency.max(1);
    usize::try_from(config.image_gen.max_parallel_requests.clamp(1, most)).unwrap_or(1)
}

/// The model of a call as the provider receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The configured id.
    pub id: String,
    /// What the API receives.
    pub api_model: String,
    /// The quality the API receives.
    pub quality: Option<String>,
    /// Takes a source image.
    pub supports_edit: bool,
    /// Most reference images.
    pub max_reference_images: u32,
    /// The provider can upscale its output.
    pub upscaler: bool,
}

impl From<&ImageModel> for Model {
    fn from(m: &ImageModel) -> Self {
        Self {
            id: m.id.to_owned(),
            api_model: m.api_model.to_owned(),
            quality: m.quality.map(str::to_owned),
            supports_edit: m.supports_edit,
            max_reference_images: m.max_reference_images,
            upscaler: m.upscaler,
        }
    }
}

impl Model {
    /// An id outside any catalog, sent verbatim (no quality, no editing).
    #[must_use]
    pub fn verbatim(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            api_model: id.to_owned(),
            quality: None,
            supports_edit: false,
            max_reference_images: 0,
            upscaler: false,
        }
    }
}

/// The provider id `config` selects (`fal` when unset).
#[must_use]
pub fn configured_provider(config: &Config) -> &str {
    config
        .image_gen
        .provider
        .as_deref()
        .unwrap_or(DEFAULT_IMAGE_PROVIDER)
}

/// The model `config` gives `provider`: `image_gen.openai.model` for
/// `openai`, else `image_gen.model`, else the provider's default.
#[must_use]
pub fn model_for(config: &Config, provider: &str) -> Option<Model> {
    let image = &config.image_gen;
    let chosen = match (&image.openai.model, &image.model) {
        (Some(model), _) if provider == "openai" => Some(model.as_str()),
        (_, model) => model.as_deref(),
    };
    let known = image_provider(provider)?;
    let id = chosen.unwrap_or(known.default_model);
    Some(
        known
            .model(id)
            .map_or_else(|| Model::verbatim(id), Model::from),
    )
}

/// An edit-capable model of `provider`, for the error of an image input
/// passed to a text-only model.
#[must_use]
pub fn edit_capable(provider: &str) -> Option<&'static str> {
    image_provider(provider)?
        .models
        .iter()
        .find(|m| m.supports_edit)
        .map(|m| m.id)
}

/// Everything a call needs.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: Model,
    /// The credential and billing.
    pub access: Access,
    /// The API's base URL.
    pub base_url: String,
}

/// The provider's API when no override applies.
#[must_use]
pub fn default_base(provider: &str) -> &'static str {
    match provider {
        "fal" => "https://fal.run",
        "openai" => "https://api.openai.com/v1",
        "openai-codex" => "https://chatgpt.com/backend-api/codex",
        "openrouter" => "https://openrouter.ai/api/v1",
        "xai" => "https://api.x.ai/v1",
        "krea" => "https://api.krea.ai/v1",
        "meta-ai" => "https://api.llama.com/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        _ => "",
    }
}

/// The call plan of the session's configuration.
///
/// # Errors
///
/// An unknown provider (fail closed, never another one) or a missing
/// credential naming what is missing.
pub fn plan(
    credentials: &Credentials,
    endpoint: &dyn Fn(&str, &str) -> String,
) -> Result<Plan, String> {
    let config = &credentials.config;
    let choice = credentials.choose(
        Category::ImageGeneration,
        config.image_gen.provider.as_deref(),
        DEFAULT_IMAGE_PROVIDER,
    );
    let provider = choice.provider;
    let model = model_for(config, &provider).ok_or_else(|| {
        format!("image_gen provider '{provider}' is not installed or not enabled")
    })?;
    let (access, base_url) = match provider.as_str() {
        "nous" => {
            let access = credentials.gateway(Category::ImageGeneration)?;
            let url = access.gateway_url.clone().unwrap_or_default();
            (access, endpoint("nous", &url))
        }
        "openai" => openai_access(credentials, endpoint)?,
        "openai-codex" => (
            credentials.subscription("openai-codex")?,
            endpoint("openai-codex", default_base("openai-codex")),
        ),
        "xai" => (
            xai_access(credentials)?,
            endpoint("xai", default_base("xai")),
        ),
        other => {
            let secret = image_provider(other)
                .and_then(|p| p.secret)
                .unwrap_or_default();
            (
                credentials.key(other, secret)?,
                endpoint(other, default_base(other)),
            )
        }
    };
    Ok(Plan {
        provider,
        model,
        access,
        base_url,
    })
}

/// `openai`: a configured custom provider's endpoint and key, else
/// `image_gen.openai.base_url` with `key_secret` (default
/// `OPENAI_API_KEY`).
fn openai_access(
    credentials: &Credentials,
    endpoint: &dyn Fn(&str, &str) -> String,
) -> Result<(Access, String), String> {
    let image = &credentials.config.image_gen.openai;
    if let Some(id) = &image.provider {
        let registry =
            pmt_providers::registry::ProviderRegistry::for_profile(&credentials.config, Vec::new())
                .map_err(|e| e.message().to_owned())?;
        let custom = registry
            .lookup(id)
            .map_err(|_| format!("image_gen.openai.provider: unknown provider '{id}'"))?;
        let base = custom
            .base_url()
            .ok_or_else(|| format!("provider '{id}' has no base_url"))?
            .to_owned();
        let access = custom
            .secret_names()
            .iter()
            .find_map(|name| credentials.key(id, name).ok())
            .unwrap_or_else(|| Access::keyless(id));
        return Ok((access, base));
    }
    let secret = image
        .key_secret
        .as_ref()
        .map_or("OPENAI_API_KEY", |s| s.0.as_str());
    let base = image
        .base_url
        .clone()
        .unwrap_or_else(|| endpoint("openai", default_base("openai")));
    Ok((credentials.key("openai", secret)?, base))
}

/// `xai`: the shared subscription rule (the `xai-oauth` sign-in, unless
/// its provider prefers the API key).
fn xai_access(credentials: &Credentials) -> Result<Access, String> {
    let registry =
        pmt_providers::registry::ProviderRegistry::for_profile(&credentials.config, Vec::new())
            .map_err(|e| e.message().to_owned())?;
    let found = pmt_providers::auth::tools::tool_credential(
        &registry,
        credentials.secrets.as_ref(),
        credentials.extras.as_ref(),
        "xai",
    )
    .map_err(|e| e.message().to_owned())?
    .ok_or_else(|| "no XAI_API_KEY or xai-oauth sign-in".to_owned())?;
    Ok(Access {
        token: Some(found.token),
        billing_source: found.billing_source,
        gateway_url: None,
    })
}
