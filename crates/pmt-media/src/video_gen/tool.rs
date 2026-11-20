//! `video_generate` (spec: web-and-media-tools, `video_generate` tool).
//!
//! The schema advertises the configured model's duration range, aspect
//! ratios and resolutions, and the optional arguments only when the model
//! honours them; the handler accepts every argument regardless (a replay
//! may carry another model's), narrowing the request to what the model
//! takes and naming what it dropped. The finished video is stored in the
//! blob store and returned as `MEDIA:<path>`.

use std::sync::Arc;

use pmt_config::Config;
use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::result::{Status, ToolResult};
use pmt_tools::usage;
use serde_json::{Value, json};

use super::catalog::{ASPECT_RATIOS, DEFAULT_ASPECT, DEFAULT_RESOLUTION, RESOLUTIONS};
use super::jobs::{self, Outcome, Output};
use super::{Extra, Plan, Request, VideoModel, configured_provider, model_for, plan};
use crate::{MediaSession, MediaSessions};

/// The tool's toolset.
pub const TOOLSET: &str = "video_gen";

struct Handler(Arc<MediaSessions>);

/// `video_generate`.
#[must_use]
pub fn registered(sessions: &Arc<MediaSessions>) -> RegisteredTool {
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(sessions))),
    )
    .with_parameters(Arc::new(parameters))
    .with_probe(Arc::new(pmt_tools::probe::FnProbe::new(
        "video_gen.credential",
        crate::probe::video,
    )))
    .with_description(Arc::new(description))
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "video_generate",
        TOOLSET,
        "Generate a short video from a text prompt (or a first frame) with the configured \
         video provider and model. Generation takes minutes. The result names the stored \
         file as MEDIA:<path>; put that line in your reply to show or send the video.",
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "What happens in the video."},
                "duration": {"type": "integer", "description": "Length in seconds."},
                "aspect_ratio": {"type": "string", "enum": ASPECT_RATIOS, "description": "Framing (default 16:9)."},
                "resolution": {"type": "string", "enum": RESOLUTIONS, "description": "Output resolution (default 720p)."},
                "image_url": {"type": "string", "description": "A first frame to animate (image-to-video; http(s) or data: URL)."},
                "reference_image_urls": {"type": "array", "items": {"type": "string"}, "description": "Reference images for subjects or style."},
                "negative_prompt": {"type": "string", "description": "What to avoid."},
                "audio": {"type": "boolean", "description": "Generate a soundtrack."},
                "seed": {"type": "integer", "description": "A fixed seed for reproducible output."},
                "upscale": {"type": "boolean", "description": "Upscale the result (default false)."}
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

fn configured_model(config: &Config) -> Option<VideoModel> {
    model_for(config, configured_provider(config))
}

/// The schema narrowed to the configured model (the full one for a
/// plugin's provider, which decides itself).
fn parameters(schema: &Value, config: &Config) -> Value {
    let mut schema = schema.clone();
    let Some(model) = configured_model(config) else {
        return schema;
    };
    let Some(properties) = schema["properties"].as_object_mut() else {
        return schema;
    };
    properties["duration"]["minimum"] = json!(model.durations.0);
    properties["duration"]["maximum"] = json!(model.durations.1);
    properties["aspect_ratio"]["enum"] = json!(model.aspect_ratios);
    properties["resolution"]["enum"] = json!(model.resolutions);
    if model.image_api.is_none() {
        properties.remove("image_url");
    }
    if model.max_references == 0 {
        properties.remove("reference_image_urls");
    }
    for extra in Extra::ALL {
        if !model.honours(extra) {
            properties.remove(extra.name());
        }
    }
    if let Some(refs) = properties.get_mut("reference_image_urls") {
        refs["maxItems"] = json!(model.max_references);
    }
    schema
}

fn description(text: &str, config: &Config) -> String {
    let provider = configured_provider(config);
    match configured_model(config) {
        Some(model) => format!(
            "{text} The configured model is {} ({provider}): {}-{} seconds.",
            model.id, model.durations.0, model.durations.1
        ),
        None => text.to_owned(),
    }
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.0.of(context)?;
            let result = session.generate_video(&arguments).await;
            usage::record(
                session.setup().usage.as_ref(),
                context,
                "video_generate",
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

/// The request of `arguments` (every argument accepted, whatever the
/// configured model advertises).
#[must_use]
pub fn request(arguments: &Value) -> Request {
    Request {
        prompt: text(arguments, "prompt").unwrap_or_default(),
        duration: arguments
            .get("duration")
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok()),
        aspect_ratio: text(arguments, "aspect_ratio").unwrap_or_else(|| DEFAULT_ASPECT.into()),
        resolution: text(arguments, "resolution").unwrap_or_else(|| DEFAULT_RESOLUTION.into()),
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
        negative_prompt: text(arguments, "negative_prompt"),
        audio: arguments.get("audio").and_then(Value::as_bool),
        seed: arguments.get("seed").and_then(Value::as_i64),
        upscale: arguments
            .get("upscale")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// What a finished job reports besides the file.
pub(super) struct Finished<'a> {
    /// The tool's name (errors).
    pub tool: &'a str,
    /// The provider's job id.
    pub job_id: &'a str,
    /// More details to report.
    pub details: Value,
}

impl MediaSession {
    /// Runs one `video_generate` call.
    pub async fn generate_video(&self, arguments: &Value) -> ToolResult {
        let request = request(arguments);
        if request.prompt.trim().is_empty() {
            return ToolResult::error("video_generate: prompt is required");
        }
        let setup = self.setup();
        if let Some(selected) = crate::plugin::selected(setup, crate::plugin::Kind::VideoGen) {
            return self.plugin_video(&selected, &request).await;
        }
        let plan = match plan(&setup.credentials, &|id, default| {
            setup.endpoint(id, default)
        }) {
            Ok(plan) => plan,
            Err(why) => return ToolResult::error(format!("video_generate unavailable: {why}")),
        };
        let settled = request.settle(&plan.model);
        let (endpoint, body) = jobs::generation(&plan, &settled.request);
        let outcome = self
            .run_job("video_generate", &plan, (&endpoint, &body))
            .await;
        let (job_id, output) = match outcome {
            Ok(done) => done,
            Err(result) => return *result,
        };
        let output = if settled.request.upscale {
            self.upscaled(&plan, output).await
        } else {
            output
        };
        let mut details = json!({
            "duration": settled.request.duration,
            "aspect_ratio": settled.request.aspect_ratio,
            "resolution": settled.request.resolution,
        });
        if !settled.ignored.is_empty() {
            details["ignored"] = json!(settled.ignored);
        }
        let finished = Finished {
            tool: "video_generate",
            job_id: &job_id,
            details,
        };
        self.video_result(&plan, &output, finished).await
    }

    /// Submits a job and waits for it: its id and output, or the result
    /// to return (failure or timeout).
    pub(super) async fn run_job(
        &self,
        tool: &str,
        plan: &Plan,
        (endpoint, body): (&str, &Value),
    ) -> Result<(String, Output), Box<ToolResult>> {
        let client = &self.setup().client;
        let job = jobs::submit(client, plan, endpoint, body)
            .await
            .map_err(|why| {
                Box::new(ToolResult::error(format!(
                    "{tool} failed ({}): {why}",
                    plan.provider
                )))
            })?;
        let timeout = std::time::Duration::from_secs(self.setup().config().video_gen.timeout_secs);
        match jobs::wait(client, plan, &job, self.timer.as_ref(), timeout).await {
            Outcome::Done(output) => Ok((job.id, output)),
            Outcome::Failed(why) => Err(Box::new(ToolResult::error(format!(
                "{tool} failed ({}, job {}): {why}",
                plan.provider, job.id
            )))),
            Outcome::TimedOut(last_error) => {
                let mut report = json!({
                    "status": "timeout",
                    "provider": plan.provider,
                    "model": plan.model.id,
                    "job_id": job.id,
                    "waited_secs": timeout.as_secs(),
                });
                if let Some(why) = last_error {
                    report["last_error"] = json!(why);
                }
                Err(Box::new(ToolResult::text(
                    Status::Timeout,
                    report.to_string(),
                )))
            }
        }
    }

    /// The upscaled video, or `output` when upscaling fails (it is an
    /// extra, never a reason to lose the video).
    async fn upscaled(&self, plan: &Plan, output: Output) -> Output {
        let Output::Public(url) = &output else {
            return output;
        };
        let body = json!({"video_url": url});
        match self
            .run_job("video_generate", plan, (jobs::FAL_UPSCALER, &body))
            .await
        {
            Ok((_, upscaled)) => upscaled,
            Err(result) => {
                tracing::warn!(error = %result.text_content(), "video upscale failed; keeping the original");
                output
            }
        }
    }

    /// Downloads and stores the video: `MEDIA:<path>` and its details.
    pub(super) async fn video_result(
        &self,
        plan: &Plan,
        output: &Output,
        finished: Finished<'_>,
    ) -> ToolResult {
        let setup = self.setup();
        let (bytes, mime) = match jobs::fetch(&setup.client, plan, output).await {
            Ok(fetched) => fetched,
            Err(why) => {
                return ToolResult::error(format!(
                    "{}: the video of job {} could not be downloaded: {why}",
                    finished.tool, finished.job_id
                ));
            }
        };
        let mime = if mime.starts_with("video/") {
            mime
        } else {
            "video/mp4".to_owned()
        };
        let path = match setup.host.store_media(&bytes, &mime) {
            Ok(path) => path,
            Err(why) => {
                return ToolResult::error(format!(
                    "{}: the video could not be stored: {why}",
                    finished.tool
                ));
            }
        };
        let mut details = finished.details;
        details["mime"] = json!(mime);
        details["provider"] = json!(plan.provider);
        details["model"] = json!(plan.model.id);
        details["job_id"] = json!(finished.job_id);
        if let Output::Public(url) = output
            && plan.provider == "xai"
        {
            // Edit and extend take this URL.
            details["video_url"] = json!(url);
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

    /// A call to the provider an enabled plugin registered.
    async fn plugin_video(
        &self,
        selected: &crate::plugin::Selected,
        request: &Request,
    ) -> ToolResult {
        match crate::plugin::generate_video(self.setup(), selected, request).await {
            Ok((plan, bytes, mime)) => {
                let path = match self.setup().host.store_media(&bytes, &mime) {
                    Ok(path) => path,
                    Err(why) => {
                        return ToolResult::error(format!(
                            "video_generate: the video could not be stored: {why}"
                        ));
                    }
                };
                let details =
                    json!({"mime": mime, "provider": plan.provider, "model": plan.model.id});
                let mut result = ToolResult::ok(format!("MEDIA:{}\n{details}", path.display()));
                usage::mark(
                    &mut result,
                    &plan.provider,
                    &plan.model.id,
                    &plan.access.billing_source,
                );
                result
            }
            Err(why) => {
                ToolResult::error(format!("video_generate failed ({}): {why}", selected.id))
            }
        }
    }
}
