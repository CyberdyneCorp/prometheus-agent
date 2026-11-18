//! How each video provider runs a generation job (spec:
//! web-and-media-tools, `video_generate` tool).
//!
//! Every provider takes a job and answers its id; the job is then polled
//! until it ends:
//! - `fal` (and `nous`, whose tool gateway fronts fal's queue):
//!   `POST <base>/<endpoint>` -> `{request_id, status_url?, response_url?}`;
//!   `GET .../requests/<id>/status` -> `IN_QUEUE|IN_PROGRESS|COMPLETED`;
//!   `GET .../requests/<id>` -> `{video: {url}}`.
//! - `xai`: `POST <base>/videos/generations|edits|extensions` ->
//!   `{request_id}`; `GET <base>/videos/<id>` -> `{status, video: {url}}`.
//! - `openrouter`: `POST <base>/videos` -> `{id}`;
//!   `GET <base>/videos/<id>` -> `{status}`; the video at
//!   `GET <base>/videos/<id>/content`.
//! - `deepinfra` (OpenAI videos API): `POST <base>/videos` -> `{id}`;
//!   `GET <base>/videos/<id>` -> `{status, data: [{url}]?}`, else the
//!   content endpoint.
//!
//! A poll that fails (network, 5xx) counts as still running: one blip
//! does not lose a ten-minute job, and the last error is reported if the
//! job then times out.

use std::time::Duration;

use serde_json::{Value, json};

use super::{MAX_VIDEO_BYTES, POLL_INTERVAL, Plan, Request, Timer};
use crate::http::{download, ok_json, read_capped, transport};

/// fal's video upscaler.
pub const FAL_UPSCALER: &str = "fal-ai/seedvr/upscale/video";

/// A submitted job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The provider's job id.
    pub id: String,
    /// Where its state is read.
    status_url: String,
    /// Where its result is read (fal).
    result_url: String,
}

/// Where a finished video is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// A URL the provider published (no credential needed).
    Public(String),
    /// An endpoint of the provider's API (sent with the credential).
    Private(String),
}

/// A poll's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Still running.
    Pending,
    /// Finished.
    Done(Output),
    /// Ended without a video.
    Failed(String),
}

/// How waiting on a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The video is ready.
    Done(Output),
    /// The job failed.
    Failed(String),
    /// The job did not end in time; the last poll error, if any.
    TimedOut(Option<String>),
}

fn authorization(plan: &Plan) -> Result<String, String> {
    let token = plan.access.bearer()?.expose();
    Ok(if plan.provider == "fal" {
        format!("Key {token}")
    } else {
        format!("Bearer {token}")
    })
}

fn is_fal_queue(plan: &Plan) -> bool {
    matches!(plan.provider.as_str(), "fal" | "nous")
}

/// The endpoint and body of a generation.
#[must_use]
pub fn generation(plan: &Plan, request: &Request) -> (String, Value) {
    let model = &plan.model;
    let image = request.image_url.is_some();
    let api = match (&model.image_api, image) {
        (Some(api), true) => api.clone(),
        _ => model.text_api.clone(),
    };
    match plan.provider.as_str() {
        "fal" | "nous" => (api.clone(), fal_body(&api, request)),
        "xai" => ("videos/generations".to_owned(), xai_body(&api, request)),
        "openrouter" => ("videos".to_owned(), openrouter_body(&api, request)),
        _ => ("videos".to_owned(), openai_body(&api, request)),
    }
}

fn put(body: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        body[key] = value;
    }
}

fn fal_body(api: &str, request: &Request) -> Value {
    let mut body = json!({"prompt": request.prompt, "aspect_ratio": request.aspect_ratio,
        "resolution": request.resolution});
    // fal's queue types durations as strings; Veo wants a unit.
    let duration = request.duration.map(|d| {
        if api.contains("veo") {
            json!(format!("{d}s"))
        } else {
            json!(d.to_string())
        }
    });
    put(&mut body, "duration", duration);
    let image_key = if api.contains("kling") {
        "start_image_url"
    } else {
        "image_url"
    };
    put(
        &mut body,
        image_key,
        request.image_url.as_ref().map(|u| json!(u)),
    );
    put(
        &mut body,
        "negative_prompt",
        request.negative_prompt.as_ref().map(|n| json!(n)),
    );
    put(&mut body, "generate_audio", request.audio.map(Value::Bool));
    put(&mut body, "seed", request.seed.map(|s| json!(s)));
    body
}

fn xai_body(api: &str, request: &Request) -> Value {
    let mut body = json!({"model": api, "prompt": request.prompt,
        "aspect_ratio": request.aspect_ratio, "resolution": request.resolution});
    put(&mut body, "duration", request.duration.map(|d| json!(d)));
    put(
        &mut body,
        "image",
        request.image_url.as_ref().map(|u| json!({"url": u})),
    );
    if !request.references.is_empty() {
        let refs: Vec<Value> = request
            .references
            .iter()
            .map(|u| json!({"url": u}))
            .collect();
        body["reference_images"] = json!(refs);
    }
    body
}

fn image_part(url: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": url}})
}

fn openrouter_body(api: &str, request: &Request) -> Value {
    let mut body = json!({"model": api, "prompt": request.prompt,
        "aspect_ratio": request.aspect_ratio, "resolution": request.resolution});
    put(&mut body, "duration", request.duration.map(|d| json!(d)));
    if let Some(url) = &request.image_url {
        let mut frame = image_part(url);
        frame["frame_type"] = json!("first_frame");
        body["frame_images"] = json!([frame]);
    }
    if !request.references.is_empty() {
        let refs: Vec<Value> = request.references.iter().map(|u| image_part(u)).collect();
        body["input_references"] = json!(refs);
    }
    put(&mut body, "generate_audio", request.audio.map(Value::Bool));
    put(&mut body, "seed", request.seed.map(|s| json!(s)));
    body
}

fn openai_body(api: &str, request: &Request) -> Value {
    let mut body = json!({"model": api, "prompt": request.prompt,
        "size": request.resolution, "aspect_ratio": request.aspect_ratio});
    put(
        &mut body,
        "seconds",
        request.duration.map(|d| json!(d.to_string())),
    );
    put(
        &mut body,
        "image_url",
        request.image_url.as_ref().map(|u| json!(u)),
    );
    put(
        &mut body,
        "negative_prompt",
        request.negative_prompt.as_ref().map(|n| json!(n)),
    );
    put(&mut body, "seed", request.seed.map(|s| json!(s)));
    body
}

/// Submits a job at `endpoint` (relative to the plan's base URL).
///
/// # Errors
///
/// The provider refused it or answered no job id.
pub async fn submit(
    client: &reqwest::Client,
    plan: &Plan,
    endpoint: &str,
    body: &Value,
) -> Result<Job, String> {
    let base = &plan.base_url;
    let reply = ok_json(
        client
            .post(format!("{base}/{endpoint}"))
            .header(reqwest::header::AUTHORIZATION, authorization(plan)?)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await,
    )
    .await?;
    let id = ["request_id", "id", "job_id"]
        .iter()
        .find_map(|k| reply[*k].as_str())
        .ok_or("the provider answered no job id")?
        .to_owned();
    Ok(if is_fal_queue(plan) {
        // fal reads requests under the endpoint's first two segments.
        let app: Vec<&str> = endpoint.split('/').take(2).collect();
        let own = format!("{base}/{}/requests/{id}", app.join("/"));
        // URLs the reply names are used only on the plan's own origin:
        // the credential goes with them.
        let same_origin = |key: &str| {
            reply[key]
                .as_str()
                .filter(|u| u.starts_with(&format!("{base}/")))
                .map(str::to_owned)
        };
        Job {
            status_url: same_origin("status_url").unwrap_or_else(|| format!("{own}/status")),
            result_url: same_origin("response_url").unwrap_or(own),
            id,
        }
    } else {
        Job {
            status_url: format!("{base}/videos/{id}"),
            result_url: format!("{base}/videos/{id}/content"),
            id,
        }
    })
}

async fn get_json(client: &reqwest::Client, plan: &Plan, url: &str) -> Result<Value, String> {
    ok_json(
        client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, authorization(plan)?)
            .send()
            .await,
    )
    .await
}

fn failure_message(reply: &Value, status: &str) -> String {
    ["/error/message", "/error", "/message", "/detail"]
        .iter()
        .find_map(|p| reply.pointer(p).and_then(Value::as_str))
        .map_or_else(
            || format!("the job ended with status '{status}'"),
            str::to_owned,
        )
}

/// Reads the job's state once.
///
/// # Errors
///
/// The poll itself failed (the job may still be running).
pub async fn poll(client: &reqwest::Client, plan: &Plan, job: &Job) -> Result<Poll, String> {
    let reply = get_json(client, plan, &job.status_url).await?;
    let status = reply["status"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if is_fal_queue(plan) {
        return match status.as_str() {
            "completed" => fal_result(client, plan, job).await,
            "in_queue" | "in_progress" | "" => Ok(Poll::Pending),
            other => Ok(Poll::Failed(failure_message(&reply, other))),
        };
    }
    Ok(match status.as_str() {
        "done" | "completed" | "succeeded" => match video_url(&reply) {
            Some(url) => Poll::Done(Output::Public(url)),
            None if plan.provider == "xai" => {
                Poll::Failed("the job finished without a video URL".to_owned())
            }
            None => Poll::Done(Output::Private(job.result_url.clone())),
        },
        "failed" | "error" | "expired" | "cancelled" | "canceled" => {
            Poll::Failed(failure_message(&reply, &status))
        }
        _ => Poll::Pending,
    })
}

fn video_url(reply: &Value) -> Option<String> {
    ["/video/url", "/data/0/url", "/videos/0/url", "/url"]
        .iter()
        .find_map(|p| reply.pointer(p).and_then(Value::as_str))
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
}

async fn fal_result(client: &reqwest::Client, plan: &Plan, job: &Job) -> Result<Poll, String> {
    match get_json(client, plan, &job.result_url).await {
        Ok(reply) => Ok(video_url(&reply).map_or_else(
            || Poll::Failed(failure_message(&reply, "completed")),
            |url| Poll::Done(Output::Public(url)),
        )),
        // A finished job whose result is an error (a 4xx) failed.
        Err(why) if why.starts_with("HTTP 4") => Ok(Poll::Failed(why)),
        Err(why) => Err(why),
    }
}

/// Polls `job` every [`POLL_INTERVAL`] until it ends or `timeout` passes
/// on `timer`.
pub async fn wait(
    client: &reqwest::Client,
    plan: &Plan,
    job: &Job,
    timer: &dyn Timer,
    timeout: Duration,
) -> Outcome {
    let started = timer.now();
    let mut last_error = None;
    loop {
        timer.sleep(POLL_INTERVAL).await;
        match poll(client, plan, job).await {
            Ok(Poll::Done(output)) => return Outcome::Done(output),
            Ok(Poll::Failed(why)) => return Outcome::Failed(why),
            Ok(Poll::Pending) => {}
            Err(why) => {
                tracing::debug!(provider = %plan.provider, job = %job.id, error = %why, "video job poll failed");
                last_error = Some(why);
            }
        }
        if timer.now().saturating_sub(started) >= timeout {
            return Outcome::TimedOut(last_error);
        }
    }
}

/// The finished video's bytes and reported MIME type.
///
/// # Errors
///
/// The download failed or exceeded [`MAX_VIDEO_BYTES`].
pub async fn fetch(
    client: &reqwest::Client,
    plan: &Plan,
    output: &Output,
) -> Result<(Vec<u8>, String), String> {
    match output {
        Output::Public(url) => download(client, url, MAX_VIDEO_BYTES).await,
        Output::Private(url) => {
            let response = client
                .get(url)
                .header(reqwest::header::AUTHORIZATION, authorization(plan)?)
                .send()
                .await
                .map_err(|e| transport(&e))?;
            read_capped(response, MAX_VIDEO_BYTES).await
        }
    }
}
