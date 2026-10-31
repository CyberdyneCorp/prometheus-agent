//! `vision_analyze` (spec: web-and-media-tools, `vision_analyze` tool).
//!
//! The image comes from an http(s) URL (through the session's URL policy,
//! at most 50 MiB), a `data:` URL or a local path read through the file
//! tools' guards. A `region` is cropped at original resolution. On a main
//! route that sees images and carries them in tool results the image itself
//! is the result; otherwise the `vision` auxiliary route answers the
//! question in text.

use std::sync::Arc;

use pmt_core::private_fs;
use serde_json::{Value, json};

use super::image::{ImageData, MAX_BYTES, too_large};
use super::routing::{Inline, inline};
use super::{NO_ROUTE, VisionSession};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, PathClaim, RegisteredTool, ResourceClaims,
    ToolDescriptor, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::files::FileSession;
use crate::result::{Content, Status, ToolResult};

/// The toolset of `vision_analyze`.
pub const TOOLSET: &str = "vision";

struct Handler(Arc<Environments>);

/// `vision_analyze`.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    vec![RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(environments))),
    )]
}

/// One call's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeArgs {
    /// http(s) URL, `data:` URL or local path.
    pub image_url: String,
    /// What to find out.
    pub question: String,
    /// `[x1, y1, x2, y2]` in original pixels.
    pub region: Option<[u32; 4]>,
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let vision = part(&self.0, context, "vision", |env| env.vision.clone())?;
            let files = self.0.of(context)?.files.clone();
            let args = match parse(&arguments) {
                Ok(args) => args,
                Err(message) => return Ok(ToolResult::error(message)),
            };
            Ok(vision.analyze(files.as_deref(), &args).await)
        })
    }
}

fn parse(arguments: &Value) -> Result<AnalyzeArgs, String> {
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let region = match arguments.get("region") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let corners: Vec<u32> = serde_json::from_value(value.clone())
                .map_err(|_| "region must be [x1, y1, x2, y2] in pixels".to_owned())?;
            Some(
                <[u32; 4]>::try_from(corners)
                    .map_err(|_| "region must be [x1, y1, x2, y2] in pixels".to_owned())?,
            )
        }
    };
    Ok(AnalyzeArgs {
        image_url: text("image_url").to_owned(),
        question: text("question").to_owned(),
        region,
    })
}

impl VisionSession {
    /// Runs one `vision_analyze` call; local paths are read through `files`.
    pub async fn analyze(&self, files: Option<&FileSession>, args: &AnalyzeArgs) -> ToolResult {
        if self.unavailable().is_some() {
            return ToolResult::error(format!("vision_analyze unavailable: {NO_ROUTE}"));
        }
        if self.native() && args.region.is_none() && self.is_attached(&args.image_url) {
            return ToolResult::ok(
                "The image is already in context: it is attached to the current user message. \
                 Look at it there, or pass a region to see part of it at full resolution.",
            );
        }
        let image = match self.prepared(files, args).await {
            Ok(image) => image,
            Err(why) => return ToolResult::error(format!("vision_analyze: {why}")),
        };
        if self.native() {
            return self.attached_result(image, &args.image_url);
        }
        self.described(image, args).await
    }

    /// The image of the call, frame-checked and cropped.
    async fn prepared(
        &self,
        files: Option<&FileSession>,
        args: &AnalyzeArgs,
    ) -> Result<ImageData, String> {
        let image = self.load(files, &args.image_url).await?;
        image.check_frames()?;
        match args.region {
            Some(region) => image.crop(region),
            None => Ok(image),
        }
    }

    async fn load(&self, files: Option<&FileSession>, source: &str) -> Result<ImageData, String> {
        if source.starts_with("data:") {
            return ImageData::from_data_url(source);
        }
        if source.starts_with("http://") || source.starts_with("https://") {
            return ImageData::sniff(self.download(source).await?);
        }
        let files = files.ok_or("local paths need the file tools' environment")?;
        let bytes = files.read_binary(source)?;
        if bytes.len() > MAX_BYTES {
            return Err(too_large());
        }
        ImageData::sniff(bytes)
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut response = self
            .setup
            .fetcher
            .get(url)
            .await
            .map_err(|err| err.message().to_owned())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {} for the image", response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "the image download failed".to_owned())?
        {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// The image itself as the result (written under the media dir).
    fn attached_result(&self, image: ImageData, source: &str) -> ToolResult {
        let image = match inline(image, source) {
            Inline::Image(image) => image,
            Inline::Note(note) => return ToolResult::ok(note),
        };
        let (width, height) = image.dimensions().unwrap_or_default();
        let extension = image.mime.trim_start_matches("image/");
        let path = self
            .setup
            .media_dir
            .join(format!("{}.{extension}", ulid::Ulid::generate()));
        let written = private_fs::create_dir(&self.setup.media_dir)
            .and_then(|()| private_fs::write_atomic(&path, &image.bytes));
        if let Err(err) = written {
            return ToolResult::error(format!("vision_analyze could not keep the image: {err}"));
        }
        ToolResult {
            status: Status::Ok,
            content: vec![
                Content::Text {
                    text: json!({"image": source, "width": width, "height": height, "mime": image.mime})
                        .to_string(),
                },
                Content::Media {
                    mime: image.mime,
                    path: path.display().to_string(),
                },
            ],
            metadata: serde_json::Map::new(),
        }
    }

    /// The `vision` auxiliary route's answer.
    async fn described(&self, image: ImageData, args: &AnalyzeArgs) -> ToolResult {
        let Some(describer) = &self.setup.describer else {
            return ToolResult::error(format!("vision_analyze unavailable: {NO_ROUTE}"));
        };
        let image = match inline(image, &args.image_url) {
            Inline::Image(image) => image,
            Inline::Note(note) => return ToolResult::ok(note),
        };
        match describer.describe(&image, &args.question).await {
            Ok(text) => ToolResult::ok(text),
            Err(why) => ToolResult::error(format!("vision_analyze failed: {why}")),
        }
    }
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "vision_analyze",
        TOOLSET,
        "Look at an image and answer a question about it. image_url is an http(s) URL, a \
         data: URL or a local file path. region [x1, y1, x2, y2] (original pixels) crops the \
         image at full resolution first, for small text or details.",
        json!({
            "type": "object",
            "properties": {
                "image_url": {"type": "string", "description": "http(s) URL, data: URL or local path."},
                "question": {"type": "string", "description": "What to find out about the image."},
                "region": {"type": "array", "items": {"type": "integer", "minimum": 0}, "minItems": 4, "maxItems": 4, "description": "[x1, y1, x2, y2] in original-image pixels."}
            },
            "required": ["image_url", "question"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    d.idempotent = true;
    // A local image path is read; a URL never matches a written path.
    d.claims = Some(ResourceClaims::reads(vec![PathClaim::Argument(
        "/image_url".into(),
    )]));
    d
}
