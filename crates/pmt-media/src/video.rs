//! `video_analyze` (spec: web-and-media-tools, `video_analyze` tool).
//!
//! A video (http(s) URL through the session's URL policy, or a local
//! path read through the session's file tools and their guards, as
//! `vision_analyze` reads images) is sent to the `vision` auxiliary route only when that route's
//! model declares video input; no other route is ever used. Videos above
//! 20 MiB carry a warning and above 50 MiB are refused before any
//! provider call.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, PathClaim, RegisteredTool, ResourceClaims,
    ToolDescriptor, ToolHandler,
};
use pmt_tools::environments::Environments;
use pmt_tools::files::FileSession;
use pmt_tools::result::ToolResult;
use pmt_tools::variants::RouteTraits;
use serde_json::{Value, json};

use crate::{MediaSession, MediaSessions};

/// The tool's toolset (off by default).
pub const TOOLSET: &str = "video";
/// Above this a result carries a size warning.
pub const WARN_BYTES: u64 = 20 << 20;
/// Above this a video is refused.
pub const MAX_BYTES: u64 = 50 << 20;
/// The auxiliary capability a session reaches when its `vision` route's
/// model takes video (`RouteTraits::auxiliary`).
pub const VIDEO_AUX: &str = "video";
/// The reason the tool gives without one.
pub const NO_ROUTE: &str = "no video-capable route";

/// The accepted containers and their MIME types.
const FORMATS: &[(&str, &str)] = &[
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mkv", "video/x-matroska"),
    ("mpeg", "video/mpeg"),
    ("mpg", "video/mpeg"),
];

/// What a description returns.
pub type DescribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// The `vision` auxiliary route of a model that takes video.
pub trait VideoDescriber: Send + Sync {
    /// The route's provider and model, for the result.
    fn route(&self) -> (String, String);
    /// How its calls bill.
    fn billing_source(&self) -> String;
    /// The route's answer to `question` about the video.
    fn describe<'a>(
        &'a self,
        video: &'a [u8],
        mime: &'a str,
        question: &'a str,
    ) -> DescribeFuture<'a>;
}

struct Handler(Arc<MediaSessions>, Arc<Environments>);

/// `video_analyze`, available only where the session reaches a
/// video-capable route; local paths are read through the file tools of
/// the session's environment in `environments`.
#[must_use]
pub fn registered(
    sessions: &Arc<MediaSessions>,
    environments: &Arc<Environments>,
) -> RegisteredTool {
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(sessions), Arc::clone(environments))),
    )
    .with_route_check(Arc::new(|_, route: &RouteTraits| {
        if route.auxiliary.contains(VIDEO_AUX) {
            Ok(())
        } else {
            Err(NO_ROUTE.to_owned())
        }
    }))
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "video_analyze",
        TOOLSET,
        "Answer a question about a video (.mp4, .webm, .mov, .avi, .mkv, .mpeg, .mpg; at \
         most 50 MiB) given by http(s) URL or local path.",
        json!({
            "type": "object",
            "properties": {
                "video_url": {"type": "string", "description": "http(s) URL or local path."},
                "question": {"type": "string", "description": "What to find out."}
            },
            "required": ["video_url", "question"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    d.idempotent = true;
    // A local video is read (and its path checked by the gate); a URL
    // never matches a written path.
    d.claims = Some(ResourceClaims::reads(vec![PathClaim::Argument(
        "/video_url".into(),
    )]));
    d.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
    d
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.0.of(context)?;
            let files = self
                .1
                .session(&context.session_id)
                .and_then(|env| env.files.clone());
            let text = |key: &str| {
                arguments
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            };
            let result = session
                .analyze_video(files.as_deref(), text("video_url"), text("question"))
                .await;
            pmt_tools::usage::record(
                session.setup().usage.as_ref(),
                context,
                "video_analyze",
                &result,
            );
            Ok(result)
        })
    }
}

/// The MIME type of an accepted video name.
#[must_use]
pub fn mime_of(name: &str) -> Option<&'static str> {
    let path = url::Url::parse(name)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map_or_else(|| name.to_owned(), |u| u.path().to_owned());
    let extension = Path::new(&path).extension()?.to_str()?.to_ascii_lowercase();
    FORMATS
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map(|(_, mime)| *mime)
}

fn mib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / f64::from(1 << 20);
    format!("{value:.1} MiB")
}

fn too_large(name: &str, bytes: u64) -> ToolResult {
    ToolResult::error(format!(
        "video_analyze: {name} is {}, over the 50 MiB limit; trim or downscale it first",
        mib(bytes)
    ))
}

impl MediaSession {
    /// Runs one `video_analyze` call; a local path is read through
    /// `files` (the session's file tools), never from the host directly.
    pub async fn analyze_video(
        &self,
        files: Option<&FileSession>,
        video_url: &str,
        question: &str,
    ) -> ToolResult {
        let Some(describer) = self.setup().video.clone() else {
            return ToolResult::error(format!("video_analyze unavailable: {NO_ROUTE}"));
        };
        let Some(mime) = mime_of(video_url) else {
            return ToolResult::error(
                "video_analyze: unsupported video type (use .mp4, .webm, .mov, .avi, .mkv, .mpeg or .mpg)",
            );
        };
        let bytes = match self.read_video(files, video_url).await {
            Ok(bytes) => bytes,
            Err(result) => return *result,
        };
        let size = bytes.len() as u64;
        let answer = match describer.describe(&bytes, mime, question).await {
            Ok(answer) => answer,
            Err(why) => return ToolResult::error(format!("video_analyze failed: {why}")),
        };
        let mut text = answer;
        if size > WARN_BYTES {
            text = format!(
                "[warning: the video is {}; large videos cost more and may be sampled by the provider]\n{text}",
                mib(size)
            );
        }
        let mut result = ToolResult::ok(text);
        let (provider, model) = describer.route();
        pmt_tools::usage::mark(&mut result, &provider, &model, &describer.billing_source());
        result
    }

    /// The video's bytes; a refusal as the error result. A local file's
    /// size is checked before it is read; a download stops at the limit.
    async fn read_video(
        &self,
        files: Option<&FileSession>,
        video_url: &str,
    ) -> Result<Vec<u8>, Box<ToolResult>> {
        let remote = video_url.starts_with("http://") || video_url.starts_with("https://");
        if !remote {
            let refused =
                |why: String| Box::new(ToolResult::error(format!("video_analyze: {why}")));
            let files = files
                .ok_or_else(|| refused("local paths need the file tools' environment".into()))?;
            let size = files.binary_len(video_url).map_err(refused)?;
            if size > MAX_BYTES {
                return Err(Box::new(too_large(video_url, size)));
            }
            return files.read_binary(video_url).map_err(refused);
        }
        let response = self.setup().fetcher.get(video_url).await.map_err(|err| {
            Box::new(ToolResult::error(format!(
                "video_analyze: {}",
                err.message()
            )))
        })?;
        match crate::http::read_capped(response, MAX_BYTES).await {
            Ok((bytes, _)) => Ok(bytes),
            Err(why) if why.starts_with("larger than") => {
                Err(Box::new(ToolResult::error(format!(
                    "video_analyze: {video_url} is over the 50 MiB limit; trim or downscale it first"
                ))))
            }
            Err(why) => Err(Box::new(ToolResult::error(format!(
                "video_analyze: {video_url}: {why}"
            )))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_containers_by_extension() {
        assert_eq!(mime_of("clip.MP4"), Some("video/mp4"));
        assert_eq!(
            mime_of("https://a.test/v/clip.webm?x=1"),
            Some("video/webm")
        );
        assert_eq!(mime_of("a/b.mpg"), Some("video/mpeg"));
        assert_eq!(mime_of("a.gif"), None);
    }
}
