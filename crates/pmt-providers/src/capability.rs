//! Capability-aware route validation (spec: model-providers,
//! Capability-aware route validation and minimum context window): a
//! request goes only to a model that can take it, and a session never
//! starts on a model whose window is below `model.min_context_window`.

use pmt_core::error::{Error, codes};

use crate::metadata::ModelMetadata;
use crate::request::Request;

/// What a request needs from its model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// Tool schemas are present.
    pub tools: bool,
    /// Image parts are present.
    pub vision: bool,
    /// Estimated prompt tokens.
    pub estimated_tokens: u64,
}

impl Needs {
    /// The needs of `request`.
    #[must_use]
    pub fn of(request: &Request) -> Self {
        Self {
            tools: !request.tools.is_empty(),
            vision: request.has_images(),
            estimated_tokens: request.estimated_tokens(),
        }
    }
}

/// Checks `meta` of `label` (`provider/model`) against `needs`.
///
/// # Errors
///
/// `model.capability_missing` naming the missing capability.
pub fn check(label: &str, meta: &ModelMetadata, needs: &Needs) -> Result<(), Error> {
    let missing = if needs.tools && !meta.supports_tools {
        Some("tool calling (supports_tools)".to_owned())
    } else if needs.vision && !meta.supports_vision {
        Some("image input (supports_vision)".to_owned())
    } else if needs.estimated_tokens > meta.context_window {
        Some(format!(
            "a context window of at least {} tokens (it has {})",
            needs.estimated_tokens, meta.context_window
        ))
    } else {
        None
    };
    match missing {
        Some(capability) => Err(Error::new(
            &codes::MODEL_CAPABILITY_MISSING,
            format!("model '{label}' lacks {capability}"),
        )
        .with_hint(format!(
            "choose another model, or if the model has it, set [models.\"{label}\"] in config.toml"
        ))),
        None => Ok(()),
    }
}

/// Refuses a session whose model window is below `min` tokens.
///
/// # Errors
///
/// `model.context_too_small` naming the model, its window, the minimum
/// and the override key.
pub fn check_min_context(label: &str, meta: &ModelMetadata, min: u64) -> Result<(), Error> {
    if meta.context_window >= min {
        return Ok(());
    }
    Err(Error::new(
        &codes::MODEL_CONTEXT_TOO_SMALL,
        format!(
            "model '{label}' has a {}-token context window, below the minimum of {min} tokens (model.min_context_window); if its window is larger, set [models.\"{label}\"] context_window",
            meta.context_window
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Message, Part, Role, Tool};

    #[test]
    fn a_route_without_tool_support_is_rejected_for_a_tool_using_request() {
        let request = Request {
            tools: vec![Tool {
                name: "read_file".into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..Request::default()
        };
        let meta = ModelMetadata {
            supports_tools: false,
            ..ModelMetadata::default()
        };
        let err = check("local/tiny", &meta, &Needs::of(&request)).unwrap_err();
        assert_eq!(err.code().code, "model.capability_missing");
        assert!(err.message().contains("supports_tools"));
        assert!(check("local/tiny", &meta, &Needs::default()).is_ok());
    }

    #[test]
    fn images_need_vision_and_large_prompts_need_a_large_window() {
        let mut message = Message::text(Role::User, "look");
        message.parts.push(Part::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            is_url: false,
        });
        let request = Request {
            messages: vec![message],
            ..Request::default()
        };
        let err = check("p/m", &ModelMetadata::default(), &Needs::of(&request)).unwrap_err();
        assert!(err.message().contains("supports_vision"));
        let small = ModelMetadata {
            context_window: 10,
            supports_vision: true,
            ..ModelMetadata::default()
        };
        assert!(
            check("p/m", &small, &Needs::of(&request))
                .unwrap_err()
                .message()
                .contains("context window")
        );
    }

    #[test]
    fn small_window_names_model_window_minimum_and_override() {
        let meta = ModelMetadata {
            context_window: 32_768,
            ..ModelMetadata::default()
        };
        let err = check_min_context("ollama/qwen3", &meta, 64_000).unwrap_err();
        assert_eq!(err.code().code, "model.context_too_small");
        assert_eq!(err.exit_code(), 78);
        for part in [
            "ollama/qwen3",
            "32768",
            "64000",
            "[models.\"ollama/qwen3\"] context_window",
        ] {
            assert!(err.message().contains(part), "{part}");
        }
        assert!(check_min_context("ollama/qwen3", &meta, 32_768).is_ok());
    }
}
