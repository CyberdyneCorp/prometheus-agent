//! The tools the `xai` video provider contributes to `video_gen` (spec:
//! web-and-media-tools, Provider-contributed media tools):
//! `xai_video_edit` and `xai_video_extend`.
//!
//! They exist only while `video_gen.provider = "xai"` and an xAI
//! credential does (their probe fails otherwise, so the surface leaves
//! them out). Their `video_url` must be the public HTTPS MP4 of a prior
//! xAI result (`video_url` in a `video_generate` result); any other URL is
//! refused before xAI is contacted.

use std::sync::Arc;

use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::probe::{FnProbe, ProbeContext, signin};
use pmt_tools::result::ToolResult;
use pmt_tools::usage;
use serde_json::{Value, json};

use super::tool::{Finished, TOOLSET};
use super::{Plan, plan};
use crate::{MediaSession, MediaSessions};

/// The longest extension xAI makes, in seconds.
pub const MAX_EXTEND_SECS: u32 = 10;

/// The default extension, in seconds.
pub const DEFAULT_EXTEND_SECS: u32 = 6;

/// The two operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// `xai_video_edit`.
    Edit,
    /// `xai_video_extend`.
    Extend,
}

impl Operation {
    fn tool(self) -> &'static str {
        match self {
            Self::Edit => "xai_video_edit",
            Self::Extend => "xai_video_extend",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Edit => "videos/edits",
            Self::Extend => "videos/extensions",
        }
    }
}

struct Handler {
    sessions: Arc<MediaSessions>,
    operation: Operation,
}

/// `xai_video_edit` and `xai_video_extend`.
#[must_use]
pub fn registered(sessions: &Arc<MediaSessions>) -> Vec<RegisteredTool> {
    [Operation::Edit, Operation::Extend]
        .into_iter()
        .map(|operation| {
            RegisteredTool::new(
                descriptor(operation),
                Owner::Builtin,
                Arc::new(Handler {
                    sessions: Arc::clone(sessions),
                    operation,
                }),
            )
            .with_probe(Arc::new(FnProbe::new("video_gen.xai", probe)))
        })
        .collect()
}

/// Present only while xAI is the configured video provider and an xAI
/// credential exists.
///
/// # Errors
///
/// What is missing.
pub fn probe(context: ProbeContext<'_>) -> Result<(), String> {
    if context.config.video_gen.provider.as_deref() != Some("xai") {
        return Err("needs video_gen.provider = \"xai\"".to_owned());
    }
    let has = context.has_secret;
    if has("XAI_API_KEY") || has(&signin("xai-oauth")) {
        Ok(())
    } else {
        Err("needs XAI_API_KEY or an xai-oauth sign-in".to_owned())
    }
}

fn descriptor(operation: Operation) -> ToolDescriptor {
    let video_url = json!({"type": "string", "description": "The public HTTPS MP4 URL of a prior xAI video result (its video_url)."});
    let (description, properties) = match operation {
        Operation::Edit => (
            "Edit a video made by xAI Imagine following an instruction. The result is \
             stored as MEDIA:<path>.",
            json!({
                "prompt": {"type": "string", "description": "How to change the video."},
                "video_url": video_url,
            }),
        ),
        Operation::Extend => (
            "Continue a video made by xAI Imagine. The result is stored as MEDIA:<path>.",
            json!({
                "prompt": {"type": "string", "description": "What happens next."},
                "video_url": video_url,
                "duration": {"type": "integer", "minimum": 1, "maximum": MAX_EXTEND_SECS,
                    "description": "Seconds to add (default 6)."},
            }),
        ),
    };
    let mut d = ToolDescriptor::new(
        operation.tool(),
        TOOLSET,
        description,
        json!({
            "type": "object",
            "properties": properties,
            "required": ["prompt", "video_url"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.idempotent = false;
    d.claims = Some(ResourceClaims::none());
    d.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
    d
}

/// Whether `url` is the public HTTPS MP4 of an xAI result: `https`, an
/// `x.ai` host and an `.mp4` path.
#[must_use]
pub fn is_xai_video(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let xai_host = parsed
        .host_str()
        .is_some_and(|h| h == "x.ai" || h.ends_with(".x.ai"));
    parsed.scheme() == "https" && xai_host && parsed.path().to_ascii_lowercase().ends_with(".mp4")
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.sessions.of(context)?;
            let result = session.xai_video(self.operation, &arguments).await;
            usage::record(
                session.setup().usage.as_ref(),
                context,
                self.operation.tool(),
                &result,
            );
            Ok(result)
        })
    }
}

impl MediaSession {
    /// Runs one `xai_video_edit` or `xai_video_extend` call.
    pub async fn xai_video(&self, operation: Operation, arguments: &Value) -> ToolResult {
        let tool = operation.tool();
        let field = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let (prompt, video_url) = (field("prompt").trim(), field("video_url").trim());
        if prompt.is_empty() {
            return ToolResult::error(format!("{tool}: prompt is required"));
        }
        if !is_xai_video(video_url) {
            return ToolResult::error(format!(
                "{tool}: video_url must be the public HTTPS MP4 URL of a prior xAI video result \
                 (the video_url of a video_generate result)"
            ));
        }
        let plan = match self.xai_plan() {
            Ok(plan) => plan,
            Err(why) => return ToolResult::error(format!("{tool} unavailable: {why}")),
        };
        let mut body = json!({"model": plan.model.text_api, "prompt": prompt,
            "video": {"url": video_url}});
        let mut details = json!({"operation": tool, "source_video_url": video_url});
        if operation == Operation::Extend {
            let asked = arguments
                .get("duration")
                .and_then(Value::as_u64)
                .and_then(|d| u32::try_from(d).ok());
            let duration = asked
                .unwrap_or(DEFAULT_EXTEND_SECS)
                .clamp(1, MAX_EXTEND_SECS);
            body["duration"] = json!(duration);
            details["duration"] = json!(duration);
        }
        let (job_id, output) = match self
            .run_job(tool, &plan, (operation.endpoint(), &body))
            .await
        {
            Ok(done) => done,
            Err(result) => return *result,
        };
        let finished = Finished {
            tool,
            job_id: &job_id,
            details,
        };
        self.video_result(&plan, &output, finished).await
    }

    /// The configured xAI plan (refused when xAI is not the configured
    /// video provider).
    fn xai_plan(&self) -> Result<Plan, String> {
        let setup = self.setup();
        if setup.config().video_gen.provider.as_deref() != Some("xai") {
            return Err("video_gen.provider is not xai".to_owned());
        }
        plan(&setup.credentials, &|id, default| {
            setup.endpoint(id, default)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::is_xai_video;

    #[test]
    fn only_public_https_mp4s_of_xai_are_accepted() {
        assert!(is_xai_video("https://vidgen.x.ai/abc/video.mp4"));
        assert!(is_xai_video("https://files-cdn.x.ai/v/1.MP4"));
        assert!(!is_xai_video("http://vidgen.x.ai/abc/video.mp4"));
        assert!(!is_xai_video("https://example.com/video.mp4"));
        assert!(!is_xai_video("https://evilx.ai/video.mp4"));
        assert!(!is_xai_video("https://vidgen.x.ai/abc/video.mov"));
        assert!(!is_xai_video("/tmp/video.mp4"));
    }
}
