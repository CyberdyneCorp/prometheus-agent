//! Image input routing (spec: web-and-media-tools, Image input routing;
//! agent-turn-loop, Image input routing).
//!
//! Every image that reaches the model goes either natively (pixels) or as a
//! `vision` auxiliary description, decided by `vision.image_input_mode`
//! when the message is first sent. Native images are made acceptable to
//! raster-only routes first: SVG is rasterized to PNG and anything else
//! becomes a text note, so one image never fails the whole request.

use pmt_config::schema::{ImageInputMode, WireFormat};
use serde::{Deserialize, Serialize};

use super::image::{ImageData, RASTER};

/// How an image reaches the model. The turn loop records it with the
/// message so a later request never changes the decision (the cached
/// prompt prefix stays byte-identical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Pixels in the request.
    Native,
    /// A description from the `vision` auxiliary route instead.
    Text,
}

/// What the routing decision looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingInputs {
    /// `vision.image_input_mode`.
    pub mode: ImageInputMode,
    /// The main route's model reports vision support.
    pub main_vision: bool,
    /// `[auxiliary.vision]` names a provider or model explicitly.
    pub aux_explicit: bool,
}

impl RoutingInputs {
    /// `auto`: pixels when the main route sees images and no explicit
    /// vision route is configured; `native` / `text` force the choice.
    #[must_use]
    pub fn delivery(self) -> Delivery {
        let native = match self.mode {
            ImageInputMode::Native => true,
            ImageInputMode::Text => false,
            ImageInputMode::Auto => self.main_vision && !self.aux_explicit,
        };
        if native {
            Delivery::Native
        } else {
            Delivery::Text
        }
    }
}

/// Whether `[auxiliary.vision]` names a provider or model.
#[must_use]
pub fn aux_explicit(config: &pmt_config::schema::Config) -> bool {
    config
        .auxiliary
        .vision
        .as_ref()
        .is_some_and(|task| task.provider.is_some() || task.model.is_some())
}

/// Whether a wire format carries images inside tool results (Anthropic
/// Messages does; Chat Completions and Responses tool outputs are text).
#[must_use]
pub fn tool_result_images(wire: WireFormat) -> bool {
    matches!(wire, WireFormat::AnthropicMessages)
}

/// An image made acceptable to a raster-only route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// A raster image to send as is.
    Image(ImageData),
    /// The image cannot be sent; this note replaces it.
    Note(String),
}

/// `image` (named `name` in notes) for a route accepting only JPEG, PNG,
/// GIF and WebP inline: raster images pass, SVG is rasterized to PNG, and
/// anything else (or a failed rasterization) becomes a note.
#[must_use]
pub fn inline(image: ImageData, name: &str) -> Inline {
    if RASTER.contains(&image.mime.as_str()) {
        return Inline::Image(image);
    }
    if image.is_svg() {
        return match image.rasterized() {
            Ok(png) => Inline::Image(png),
            Err(why) => Inline::Note(format!(
                "[image {name} could not be shown to the model: {why}]"
            )),
        };
    }
    Inline::Note(format!(
        "[image {name} ({}) could not be shown to the model: unsupported format]",
        image.mime
    ))
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::{ImageInputMode, WireFormat};

    use super::{Delivery, RoutingInputs, tool_result_images};

    fn inputs(mode: ImageInputMode, main_vision: bool, aux_explicit: bool) -> RoutingInputs {
        RoutingInputs {
            mode,
            main_vision,
            aux_explicit,
        }
    }

    #[test]
    fn auto_attaches_pixels_only_to_a_vision_route_without_an_explicit_vision_task() {
        use ImageInputMode::{Auto, Native, Text};
        assert_eq!(inputs(Auto, true, false).delivery(), Delivery::Native);
        assert_eq!(inputs(Auto, false, false).delivery(), Delivery::Text);
        assert_eq!(inputs(Auto, true, true).delivery(), Delivery::Text);
        assert_eq!(inputs(Native, false, true).delivery(), Delivery::Native);
        assert_eq!(inputs(Text, true, false).delivery(), Delivery::Text);
    }

    #[test]
    fn only_anthropic_messages_carries_images_in_tool_results() {
        assert!(tool_result_images(WireFormat::AnthropicMessages));
        assert!(!tool_result_images(WireFormat::ChatCompletions));
        assert!(!tool_result_images(WireFormat::Responses));
    }
}
