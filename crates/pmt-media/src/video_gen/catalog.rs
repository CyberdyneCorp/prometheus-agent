//! What each built-in video provider's models accept (spec:
//! web-and-media-tools, `video_generate` tool): the duration range, the
//! aspect ratios and resolutions, and which optional arguments the model
//! honours. The tool's schema is narrowed to the configured model; an id
//! outside the catalog is sent verbatim with the provider's general
//! limits.

/// Every aspect ratio the tool knows, in the tool's order.
pub const ASPECT_RATIOS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3", "21:9"];

/// Every resolution the tool knows, in the tool's order.
pub const RESOLUTIONS: &[&str] = &["480p", "540p", "720p", "768p", "1080p"];

/// The default aspect ratio.
pub const DEFAULT_ASPECT: &str = "16:9";

/// The default resolution.
pub const DEFAULT_RESOLUTION: &str = "720p";

/// The provider used when nothing selects one.
pub const DEFAULT_PROVIDER: &str = "fal";

/// The built-in providers.
pub const PROVIDERS: &[&str] = &["fal", "xai", "openrouter", "deepinfra", "nous"];

/// One model's limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoModel {
    /// The configured id.
    pub id: String,
    /// The text-to-video endpoint or API model.
    pub text_api: String,
    /// The image-to-video endpoint or API model (`None`: no image input).
    pub image_api: Option<String>,
    /// Shortest and longest clip, in seconds.
    pub durations: (u32, u32),
    /// Accepted aspect ratios.
    pub aspect_ratios: Vec<&'static str>,
    /// Accepted resolutions.
    pub resolutions: Vec<&'static str>,
    /// Most reference images (0: none).
    pub max_references: u32,
    /// The optional arguments it honours.
    pub extras: Vec<Extra>,
}

/// An optional argument only some models honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    /// `negative_prompt`.
    NegativePrompt,
    /// `audio`.
    Audio,
    /// `seed`.
    Seed,
    /// `upscale` (the provider can upscale the result).
    Upscale,
}

impl Extra {
    /// Every extra, in schema order.
    pub const ALL: [Self; 4] = [Self::NegativePrompt, Self::Audio, Self::Seed, Self::Upscale];

    /// The argument's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NegativePrompt => "negative_prompt",
            Self::Audio => "audio",
            Self::Seed => "seed",
            Self::Upscale => "upscale",
        }
    }
}

/// One catalog row (static data).
struct Row {
    provider: &'static str,
    id: &'static str,
    text_api: &'static str,
    image_api: Option<&'static str>,
    durations: (u32, u32),
    aspect_ratios: &'static [&'static str],
    resolutions: &'static [&'static str],
    max_references: u32,
    extras: &'static [Extra],
}

use Extra::{Audio, NegativePrompt, Seed, Upscale};

const FAL_ASPECTS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4"];
const XAI_ASPECTS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3"];

const ROWS: &[Row] = &[
    Row {
        provider: "fal",
        id: "pixverse-v6",
        text_api: "fal-ai/pixverse/v6/text-to-video",
        image_api: Some("fal-ai/pixverse/v6/image-to-video"),
        durations: (1, 15),
        aspect_ratios: FAL_ASPECTS,
        resolutions: &["540p", "720p", "1080p"],
        max_references: 0,
        extras: &[NegativePrompt, Audio, Seed, Upscale],
    },
    Row {
        provider: "fal",
        id: "veo3.1",
        text_api: "fal-ai/veo3.1",
        image_api: Some("fal-ai/veo3.1/image-to-video"),
        durations: (4, 8),
        aspect_ratios: &["16:9", "9:16"],
        resolutions: &["720p", "1080p"],
        max_references: 0,
        extras: &[NegativePrompt, Audio, Seed, Upscale],
    },
    Row {
        provider: "fal",
        id: "kling-v3",
        text_api: "fal-ai/kling-video/v3/standard/text-to-video",
        image_api: Some("fal-ai/kling-video/v3/standard/image-to-video"),
        durations: (3, 15),
        aspect_ratios: &["16:9", "9:16", "1:1"],
        resolutions: &["720p", "1080p"],
        max_references: 0,
        extras: &[NegativePrompt, Audio, Upscale],
    },
    Row {
        provider: "xai",
        id: "grok-imagine-video",
        text_api: "grok-imagine-video",
        image_api: Some("grok-imagine-video-1.5"),
        durations: (1, 15),
        aspect_ratios: XAI_ASPECTS,
        resolutions: &["480p", "720p"],
        max_references: 7,
        extras: &[],
    },
    Row {
        provider: "openrouter",
        id: "minimax/hailuo-3-max",
        text_api: "minimax/hailuo-3-max",
        image_api: Some("minimax/hailuo-3-max"),
        durations: (5, 15),
        aspect_ratios: &["21:9", "16:9", "4:3", "1:1", "3:4", "9:16"],
        resolutions: &["480p", "768p"],
        max_references: 3,
        extras: &[],
    },
    Row {
        provider: "openrouter",
        id: "google/veo-3.1",
        text_api: "google/veo-3.1",
        image_api: Some("google/veo-3.1"),
        durations: (4, 8),
        aspect_ratios: &["16:9", "9:16"],
        resolutions: &["720p", "1080p"],
        max_references: 3,
        extras: &[Audio, Seed],
    },
    Row {
        provider: "deepinfra",
        id: "Wan-AI/Wan2.1-T2V-14B",
        text_api: "Wan-AI/Wan2.1-T2V-14B",
        image_api: Some("Wan-AI/Wan2.1-I2V-14B-720P"),
        durations: (1, 10),
        aspect_ratios: &["16:9", "9:16", "1:1"],
        resolutions: &["480p", "720p", "1080p"],
        max_references: 0,
        extras: &[NegativePrompt, Seed],
    },
];

/// The default model of each provider (`nous` fronts fal's queue).
#[must_use]
pub fn default_model(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "fal" | "nous" => "pixverse-v6",
        "xai" => "grok-imagine-video",
        "openrouter" => "minimax/hailuo-3-max",
        "deepinfra" => "Wan-AI/Wan2.1-T2V-14B",
        _ => return None,
    })
}

/// The catalog provider whose rows `provider` uses.
fn rows_of(provider: &str) -> &str {
    if provider == "nous" { "fal" } else { provider }
}

/// The model `id` of `provider`: its catalog row, else the id sent
/// verbatim with every tool value allowed (`None`: not a built-in
/// provider).
#[must_use]
pub fn model(provider: &str, id: &str) -> Option<VideoModel> {
    default_model(provider)?;
    let catalog = rows_of(provider);
    let row = ROWS.iter().find(|r| r.provider == catalog && r.id == id);
    Some(row.map_or_else(|| VideoModel::verbatim(id), from_row))
}

fn from_row(row: &Row) -> VideoModel {
    VideoModel {
        id: row.id.to_owned(),
        text_api: row.text_api.to_owned(),
        image_api: row.image_api.map(str::to_owned),
        durations: row.durations,
        aspect_ratios: row.aspect_ratios.to_vec(),
        resolutions: row.resolutions.to_vec(),
        max_references: row.max_references,
        extras: row.extras.to_vec(),
    }
}

impl VideoModel {
    /// Whether the model honours `extra`.
    #[must_use]
    pub fn honours(&self, extra: Extra) -> bool {
        self.extras.contains(&extra)
    }

    /// An id outside the catalog: the API receives it as is and decides.
    #[must_use]
    pub fn verbatim(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            text_api: id.to_owned(),
            image_api: Some(id.to_owned()),
            durations: (1, 30),
            aspect_ratios: ASPECT_RATIOS.to_vec(),
            resolutions: RESOLUTIONS.to_vec(),
            max_references: 0,
            extras: Vec::new(),
        }
    }

    /// `requested` clamped into the model's duration range.
    #[must_use]
    pub fn duration(&self, requested: Option<u32>) -> Option<u32> {
        requested.map(|d| d.clamp(self.durations.0, self.durations.1))
    }

    /// `requested` when the model accepts it, else the closest default
    /// it does (replays may carry values another model advertised).
    #[must_use]
    pub fn aspect_ratio(&self, requested: &str) -> &'static str {
        pick(&self.aspect_ratios, requested, DEFAULT_ASPECT)
    }

    /// `requested` when the model offers it, else the offered resolution
    /// nearest in height.
    #[must_use]
    pub fn resolution(&self, requested: &str) -> &'static str {
        let height = |r: &str| r.trim_end_matches('p').parse::<i64>().ok();
        let target = height(requested).unwrap_or(720);
        self.resolutions
            .iter()
            .min_by_key(|r| height(r).map_or(i64::MAX, |h| (h - target).abs()))
            .copied()
            .unwrap_or(DEFAULT_RESOLUTION)
    }
}

fn pick(accepted: &[&'static str], requested: &str, default: &'static str) -> &'static str {
    accepted
        .iter()
        .find(|v| **v == requested)
        .or_else(|| accepted.iter().find(|v| **v == default))
        .or_else(|| accepted.first())
        .copied()
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_values_fall_back_to_what_the_model_accepts() {
        let veo = model("fal", "veo3.1").unwrap();
        assert_eq!(veo.duration(Some(30)), Some(8));
        assert_eq!(veo.duration(Some(1)), Some(4));
        assert_eq!(veo.aspect_ratio("9:16"), "9:16");
        assert_eq!(veo.aspect_ratio("21:9"), "16:9");
        assert_eq!(veo.resolution("480p"), "720p");
        let hailuo = model("openrouter", "minimax/hailuo-3-max").unwrap();
        assert_eq!(hailuo.resolution("1080p"), "768p");
        assert_eq!(hailuo.resolution("540p"), "480p");
    }

    #[test]
    fn nous_uses_the_fal_catalog_and_unknown_ids_go_verbatim() {
        assert_eq!(model("nous", "veo3.1").unwrap().text_api, "fal-ai/veo3.1");
        let custom = model("openrouter", "acme/new-video").unwrap();
        assert_eq!(custom.text_api, "acme/new-video");
        assert_eq!(custom.aspect_ratios, ASPECT_RATIOS);
        assert!(model("sora", "x").is_none());
    }
}
