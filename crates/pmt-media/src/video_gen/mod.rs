//! Video generation (spec: web-and-media-tools, `video_generate` tool;
//! Provider-contributed media tools).
//!
//! [`plan`] settles the provider, model and credential of a call from
//! `[video_gen]` (the agent never picks either); [`jobs`] speaks each
//! provider's asynchronous job API and polls a job every
//! [`POLL_INTERVAL`] until it ends or `video_gen.timeout_secs` passes;
//! [`tool`] is `video_generate` and [`xai`] the tools the `xai` provider
//! contributes while it is the configured one.

pub mod catalog;
pub mod jobs;
pub mod tool;
pub mod xai;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use pmt_config::Config;
use pmt_providers::auth::gateway::Category;

pub use catalog::{Extra, VideoModel};

use crate::access::{Access, Credentials};

/// How often a job is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Largest finished video downloaded.
pub const MAX_VIDEO_BYTES: u64 = 512 << 20;

/// A sleep of a [`Timer`].
pub type Sleep<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The clock job polling waits on (tests drive a fake one).
pub trait Timer: Send + Sync {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
    /// Waits `period`.
    fn sleep(&self, period: Duration) -> Sleep<'_>;
}

/// The real clock.
pub struct SystemTimer(Instant);

impl Default for SystemTimer {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Timer for SystemTimer {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&self, period: Duration) -> Sleep<'_> {
        Box::pin(tokio::time::sleep(period))
    }
}

/// The provider id `config` selects (`fal` when unset).
#[must_use]
pub fn configured_provider(config: &Config) -> &str {
    config
        .video_gen
        .provider
        .as_deref()
        .unwrap_or(catalog::DEFAULT_PROVIDER)
}

/// The model `config` gives `provider` (`None`: not a built-in
/// provider).
#[must_use]
pub fn model_for(config: &Config, provider: &str) -> Option<VideoModel> {
    let id = match &config.video_gen.model {
        Some(id) => id.as_str(),
        None => catalog::default_model(provider)?,
    };
    catalog::model(provider, id)
}

/// Everything a call needs.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The provider id.
    pub provider: String,
    /// The model.
    pub model: VideoModel,
    /// The credential and billing.
    pub access: Access,
    /// The job API's base URL.
    pub base_url: String,
}

/// The provider's job API when no override applies.
#[must_use]
pub fn default_base(provider: &str) -> &'static str {
    match provider {
        "fal" => "https://queue.fal.run",
        "xai" => "https://api.x.ai/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        _ => "",
    }
}

/// The secret holding a provider's API key.
fn secret_of(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "fal" => "FAL_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "deepinfra" => "DEEPINFRA_API_KEY",
        _ => return None,
    })
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
        Category::VideoGeneration,
        config.video_gen.provider.as_deref(),
        catalog::DEFAULT_PROVIDER,
    );
    let provider = choice.provider;
    let model = model_for(config, &provider)
        .ok_or_else(|| crate::plugin::not_installed(crate::plugin::Kind::VideoGen, &provider))?;
    let (access, base_url) = match provider.as_str() {
        "nous" => {
            let access = credentials.gateway(Category::VideoGeneration)?;
            let url = access.gateway_url.clone().unwrap_or_default();
            (access, endpoint("nous", &url))
        }
        "xai" => (
            crate::image::xai_access(credentials)?,
            endpoint("xai", default_base("xai")),
        ),
        other => (
            credentials.key(other, secret_of(other).unwrap_or_default())?,
            endpoint(other, default_base(other)),
        ),
    };
    Ok(Plan {
        provider,
        model,
        access,
        base_url,
    })
}

/// One `video_generate` request as the tool received it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// What to film.
    pub prompt: String,
    /// Seconds asked for.
    pub duration: Option<u32>,
    /// The aspect ratio asked for.
    pub aspect_ratio: String,
    /// The resolution asked for.
    pub resolution: String,
    /// A first frame (image-to-video).
    pub image_url: Option<String>,
    /// Reference images.
    pub references: Vec<String>,
    /// What to avoid.
    pub negative_prompt: Option<String>,
    /// Generate sound.
    pub audio: Option<bool>,
    /// A fixed seed.
    pub seed: Option<i64>,
    /// Upscale the result.
    pub upscale: bool,
}

/// A request narrowed to what the model honours.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    /// The request with unsupported values replaced or dropped.
    pub request: Request,
    /// The arguments the model does not honour, which were dropped.
    pub ignored: Vec<&'static str>,
}

impl Request {
    /// The request as `model` takes it: the duration clamped into its
    /// range, an aspect ratio or resolution it lacks replaced by one it
    /// has, and arguments it does not honour dropped (and named).
    #[must_use]
    pub fn settle(&self, model: &VideoModel) -> Settled {
        let mut request = self.clone();
        let mut ignored = Vec::new();
        request.duration = model.duration(self.duration);
        model
            .aspect_ratio(&self.aspect_ratio)
            .clone_into(&mut request.aspect_ratio);
        model
            .resolution(&self.resolution)
            .clone_into(&mut request.resolution);
        let mut drop_if = |unsupported: bool, name: &'static str, present: bool| {
            if unsupported && present {
                ignored.push(name);
            }
            unsupported && present
        };
        if drop_if(
            model.image_api.is_none(),
            "image_url",
            request.image_url.is_some(),
        ) {
            request.image_url = None;
        }
        let refs = !request.references.is_empty();
        if drop_if(model.max_references == 0, "reference_image_urls", refs) {
            request.references.clear();
        }
        request
            .references
            .truncate(usize::try_from(model.max_references).unwrap_or(usize::MAX));
        let lacks = |extra| !model.honours(extra);
        let negative = request.negative_prompt.is_some();
        if drop_if(lacks(Extra::NegativePrompt), "negative_prompt", negative) {
            request.negative_prompt = None;
        }
        if drop_if(lacks(Extra::Audio), "audio", request.audio.is_some()) {
            request.audio = None;
        }
        if drop_if(lacks(Extra::Seed), "seed", request.seed.is_some()) {
            request.seed = None;
        }
        if drop_if(lacks(Extra::Upscale), "upscale", request.upscale) {
            request.upscale = false;
        }
        Settled { request, ignored }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_keeps_only_what_the_model_honours() {
        let model = catalog::model("xai", "grok-imagine-video").unwrap();
        let request = Request {
            prompt: "a fox".into(),
            duration: Some(40),
            aspect_ratio: "21:9".into(),
            resolution: "1080p".into(),
            negative_prompt: Some("blur".into()),
            seed: Some(7),
            references: vec!["https://a.test/1.png".into()],
            ..Request::default()
        };
        let settled = request.settle(&model);
        assert_eq!(settled.request.duration, Some(15));
        assert_eq!(settled.request.aspect_ratio, "16:9");
        assert_eq!(settled.request.resolution, "720p");
        assert_eq!(settled.request.negative_prompt, None);
        assert_eq!(settled.request.references.len(), 1);
        assert_eq!(settled.ignored, ["negative_prompt", "seed"]);
    }
}
