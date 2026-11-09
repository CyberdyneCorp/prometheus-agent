//! `image_generate` (spec: web-and-media-tools, `image_generate` tool).
//!
//! The schema offers `image_url` and `reference_image_urls` only when the
//! configured model edits, and the description says so; the result is the
//! stored image as `MEDIA:<path>` with its MIME type, provider, model and
//! the size actually produced.

use std::sync::Arc;

use pmt_config::Config;
use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::result::ToolResult;
use pmt_tools::usage;
use serde_json::{Value, json};

use super::providers::{Aspect, Generated, Request, generate};
use super::{Model, configured_provider, edit_capable, model_for, plan};
use crate::{MediaSession, MediaSessions};

/// The tool's toolset.
pub const TOOLSET: &str = "image_gen";

struct Handler(Arc<MediaSessions>);

/// `image_generate`.
#[must_use]
pub fn registered(sessions: &Arc<MediaSessions>) -> RegisteredTool {
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(sessions))),
    )
    .with_parameters(Arc::new(parameters))
    .with_probe(Arc::new(pmt_tools::probe::FnProbe::new(
        "image_gen.credential",
        crate::probe::image,
    )))
    .with_description(Arc::new(description))
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "image_generate",
        TOOLSET,
        "Generate an image from a text prompt with the configured image provider and \
         model. The result names the stored file as MEDIA:<path>; put that line in your \
         reply to show or send the image.",
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "What to draw."},
                "aspect_ratio": {"type": "string", "enum": ["landscape", "square", "portrait"], "description": "Framing (default landscape)."},
                "upscale": {"type": "boolean", "description": "Upscale the result where the provider can (default false)."},
                "image_url": {"type": "string", "description": "A source image to edit (http(s) or data: URL)."},
                "reference_image_urls": {"type": "array", "items": {"type": "string"}, "description": "Reference images."}
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.idempotent = false;
    d.claims = Some(ResourceClaims::none());
    d.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
    d
}

fn configured_model(config: &Config) -> Option<Model> {
    model_for(config, configured_provider(config))
}

/// The editing arguments only when the configured model edits, with the
/// reference list capped at its limit.
fn parameters(schema: &Value, config: &Config) -> Value {
    let mut schema = schema.clone();
    let Some(properties) = schema["properties"].as_object_mut() else {
        return schema;
    };
    match configured_model(config) {
        Some(model) if model.supports_edit => {
            if let Some(refs) = properties.get_mut("reference_image_urls") {
                refs["maxItems"] = json!(model.max_reference_images);
            }
        }
        _ => {
            properties.remove("image_url");
            properties.remove("reference_image_urls");
        }
    }
    schema
}

/// The description states, at surface build, whether editing is
/// available for the configured model.
fn description(text: &str, config: &Config) -> String {
    let provider = configured_provider(config);
    match configured_model(config) {
        Some(model) if model.supports_edit => format!(
            "{text} Editing is available with {} ({provider}): pass image_url to edit it and up to {} reference_image_urls.",
            model.id, model.max_reference_images
        ),
        Some(model) => format!(
            "{text} The configured model {} ({provider}) only generates from text; it cannot edit images.",
            model.id
        ),
        None => text.to_owned(),
    }
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.0.of(context)?;
            let result = session.generate_image(&arguments).await;
            usage::record(
                session.setup().usage.as_ref(),
                context,
                "image_generate",
                &result,
            );
            Ok(result)
        })
    }
}

fn text(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn request(arguments: &Value) -> Result<Request, String> {
    let aspect = match arguments.get("aspect_ratio").and_then(Value::as_str) {
        None => Aspect::Landscape,
        Some(value) => Aspect::parse(value).ok_or_else(|| {
            format!("aspect_ratio must be landscape, square or portrait, not '{value}'")
        })?,
    };
    Ok(Request {
        prompt: text(arguments, "prompt").unwrap_or_default(),
        aspect,
        image_url: text(arguments, "image_url"),
        references: arguments
            .get("reference_image_urls")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        upscale: arguments
            .get("upscale")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The pixel size of an image and its sniffed MIME type.
fn inspect(bytes: &[u8], reported: &str) -> (Option<(u32, u32)>, String) {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format();
    let Ok(reader) = reader else {
        return (None, reported.to_owned());
    };
    let mime = reader
        .format()
        .map_or_else(|| reported.to_owned(), |f| f.to_mime_type().to_owned());
    (reader.into_dimensions().ok(), mime)
}

impl MediaSession {
    /// Runs one `image_generate` call.
    pub async fn generate_image(&self, arguments: &Value) -> ToolResult {
        let request = match request(arguments) {
            Ok(request) => request,
            Err(why) => return ToolResult::error(format!("image_generate: {why}")),
        };
        let setup = self.setup();
        let plan = match plan(&setup.credentials, &|id, default| {
            setup.endpoint(id, default)
        }) {
            Ok(plan) => plan,
            Err(why) => return ToolResult::error(format!("image_generate unavailable: {why}")),
        };
        let has_inputs = request.image_url.is_some() || !request.references.is_empty();
        if has_inputs && !plan.model.supports_edit {
            let hint = edit_capable(&plan.provider)
                .map(|m| format!("; set image_gen.model to an edit-capable model such as {m}"))
                .unwrap_or_default();
            return ToolResult::error(format!(
                "image_generate: {} cannot take image inputs{hint}",
                plan.model.id
            ));
        }
        if request.references.len() > plan.model.max_reference_images as usize {
            return ToolResult::error(format!(
                "image_generate: {} takes at most {} reference images",
                plan.model.id, plan.model.max_reference_images
            ));
        }
        let Ok(_slot) = self.image_slots.acquire().await else {
            return ToolResult::error("image_generate: the session is closing");
        };
        match generate(&setup.client, &plan, &request).await {
            Ok(generated) => self.image_result(&plan, &request, generated),
            Err(why) => {
                ToolResult::error(format!("image_generate failed ({}): {why}", plan.provider))
            }
        }
    }

    fn image_result(
        &self,
        plan: &super::Plan,
        request: &Request,
        generated: Generated,
    ) -> ToolResult {
        let (dimensions, mime) = inspect(&generated.bytes, &generated.mime);
        let path = match self.setup().host.store_media(&generated.bytes, &mime) {
            Ok(path) => path,
            Err(why) => {
                return ToolResult::error(format!(
                    "image_generate: the image could not be stored: {why}"
                ));
            }
        };
        let produced = dimensions
            .map(|(w, h)| format!("{w}x{h}"))
            .or(generated.size);
        let mut details = json!({
            "mime": mime,
            "provider": plan.provider,
            "model": plan.model.id,
            "size": produced,
            "requested_size": request.aspect.size(),
        });
        if plan.model.quality.is_some() || generated.quality.is_some() {
            details["requested_quality"] = json!(plan.model.quality);
            details["quality"] = json!(generated.quality);
        }
        if request.upscale {
            details["upscaled"] = json!(generated.upscaled);
            if !plan.model.upscaler {
                details["upscale_ignored"] = json!(format!("{} has no upscaler", plan.model.id));
            }
        }
        let mut result = ToolResult::ok(format!("MEDIA:{}\n{details}", path.display()));
        usage::mark(
            &mut result,
            &plan.provider,
            &plan.model.id,
            &plan.access.billing_source,
        );
        result
    }
}
