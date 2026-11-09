//! The built-in image generation providers and their model catalogs (spec:
//! web-and-media-tools, Built-in image generation providers and models).
//!
//! Each model declares whether it edits (`supports_edit`), how many
//! reference images it takes and whether the provider can upscale its
//! output; `image_generate` advertises its editing arguments from these
//! declarations and config validation refuses a model id the chosen
//! provider does not list (except where the catalog is open).

/// One catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageModel {
    /// The id `image_gen.model` names.
    pub id: &'static str,
    /// The model the provider's API receives.
    pub api_model: &'static str,
    /// The quality the provider's API receives (GPT Image models).
    pub quality: Option<&'static str>,
    /// Takes a source image to edit.
    pub supports_edit: bool,
    /// Most reference images per request.
    pub max_reference_images: u32,
    /// The provider can upscale this model's output.
    pub upscaler: bool,
}

/// One built-in provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageProvider {
    /// The id `image_gen.provider` names.
    pub id: &'static str,
    /// The secret holding its key (`None`: a sign-in or the gateway).
    pub secret: Option<&'static str>,
    /// The model used when `image_gen.model` is unset.
    pub default_model: &'static str,
    /// The catalog.
    pub models: &'static [ImageModel],
    /// Any id is accepted (a live catalog or a custom endpoint).
    pub open_catalog: bool,
}

impl ImageProvider {
    /// The catalog entry of `id`.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&'static ImageModel> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The ids of the catalog.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.models.iter().map(|m| m.id).collect()
    }
}

const fn model(id: &'static str, edit: bool, references: u32, upscaler: bool) -> ImageModel {
    ImageModel {
        id,
        api_model: id,
        quality: None,
        supports_edit: edit,
        max_reference_images: references,
        upscaler,
    }
}

const fn gpt(id: &'static str, api_model: &'static str, quality: &'static str) -> ImageModel {
    ImageModel {
        id,
        api_model,
        quality: Some(quality),
        supports_edit: true,
        max_reference_images: 16,
        upscaler: false,
    }
}

/// fal.ai: GPT Image quality is pinned to `medium`.
const FAL: &[ImageModel] = &[
    model("fal-ai/flux-2/klein/9b", true, 4, true),
    model("fal-ai/flux-2-pro", true, 8, true),
    model("fal-ai/z-image/turbo", false, 0, true),
    model("fal-ai/nano-banana-pro", true, 14, false),
    gpt("fal-ai/gpt-image-1.5", "fal-ai/gpt-image-1.5", "medium"),
    gpt("fal-ai/gpt-image-2", "fal-ai/gpt-image-2", "medium"),
    gpt(
        "fal-ai/gpt-image-2.5/flare",
        "fal-ai/gpt-image-2.5/flare",
        "medium",
    ),
    gpt(
        "fal-ai/gpt-image-2.5/sunburst",
        "fal-ai/gpt-image-2.5/sunburst",
        "medium",
    ),
    model("fal-ai/ideogram/v3", false, 0, true),
    model("fal-ai/recraft/v4/pro/text-to-image", false, 0, true),
    model("fal-ai/qwen-image", false, 0, true),
    model("fal-ai/krea/v2/medium/text-to-image", false, 0, true),
    model("fal-ai/krea/v2/large/text-to-image", false, 0, true),
];

/// `OpenAI`: catalog ids carry the quality.
const OPENAI: &[ImageModel] = &[
    gpt("gpt-image-2-low", "gpt-image-2", "low"),
    gpt("gpt-image-2-medium", "gpt-image-2", "medium"),
    gpt("gpt-image-2-high", "gpt-image-2", "high"),
    gpt("gpt-image-1.5-low", "gpt-image-1.5", "low"),
    gpt("gpt-image-1.5-medium", "gpt-image-1.5", "medium"),
    gpt("gpt-image-1.5-high", "gpt-image-1.5", "high"),
];

const CODEX: &[ImageModel] = &[
    gpt("gpt-image-2-low", "gpt-image-2", "low"),
    gpt("gpt-image-2-medium", "gpt-image-2", "medium"),
    gpt("gpt-image-2-high", "gpt-image-2", "high"),
];

const OPENROUTER: &[ImageModel] = &[
    model("google/gemini-2.5-flash-image", true, 3, false),
    model("google/gemini-3-pro-image-preview", true, 14, false),
    model("openai/gpt-5-image", true, 16, false),
];

const XAI: &[ImageModel] = &[
    model("grok-imagine-image", true, 1, false),
    model("grok-2-image", false, 0, false),
];

const KREA: &[ImageModel] = &[
    model("krea-2-medium", false, 0, true),
    model("krea-2-large", false, 0, true),
];

const META_AI: &[ImageModel] = &[model("muse-image-1.0", true, 4, false)];

const DEEPINFRA: &[ImageModel] = &[
    model("black-forest-labs/FLUX-2-dev", true, 4, false),
    model("black-forest-labs/FLUX-1-schnell", false, 0, false),
];

/// Every built-in image provider.
pub const IMAGE_PROVIDERS: &[ImageProvider] = &[
    ImageProvider {
        id: "fal",
        secret: Some("FAL_KEY"),
        default_model: "fal-ai/flux-2/klein/9b",
        models: FAL,
        open_catalog: false,
    },
    ImageProvider {
        id: "openai",
        secret: Some("OPENAI_API_KEY"),
        default_model: "gpt-image-2-medium",
        models: OPENAI,
        open_catalog: false,
    },
    ImageProvider {
        id: "openai-codex",
        secret: None,
        default_model: "gpt-image-2-medium",
        models: CODEX,
        open_catalog: false,
    },
    ImageProvider {
        id: "openrouter",
        secret: Some("OPENROUTER_API_KEY"),
        default_model: "google/gemini-2.5-flash-image",
        models: OPENROUTER,
        open_catalog: true,
    },
    ImageProvider {
        id: "xai",
        secret: Some("XAI_API_KEY"),
        default_model: "grok-imagine-image",
        models: XAI,
        open_catalog: false,
    },
    ImageProvider {
        id: "krea",
        secret: Some("KREA_API_KEY"),
        default_model: "krea-2-medium",
        models: KREA,
        open_catalog: false,
    },
    ImageProvider {
        id: "meta-ai",
        secret: Some("META_AI_API_KEY"),
        default_model: "muse-image-1.0",
        models: META_AI,
        open_catalog: false,
    },
    ImageProvider {
        id: "deepinfra",
        secret: Some("DEEPINFRA_API_KEY"),
        default_model: "black-forest-labs/FLUX-2-dev",
        models: DEEPINFRA,
        open_catalog: false,
    },
    ImageProvider {
        id: "nous",
        secret: None,
        default_model: "fal-ai/flux-2/klein/9b",
        models: FAL,
        open_catalog: false,
    },
];

/// The provider used when `image_gen.provider` is unset and nothing is
/// detected.
pub const DEFAULT_IMAGE_PROVIDER: &str = "fal";

/// The built-in provider `id`.
#[must_use]
pub fn image_provider(id: &str) -> Option<&'static ImageProvider> {
    IMAGE_PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_model_is_in_its_catalog() {
        for provider in IMAGE_PROVIDERS {
            assert!(
                provider.model(provider.default_model).is_some(),
                "{}",
                provider.id
            );
        }
    }
}
