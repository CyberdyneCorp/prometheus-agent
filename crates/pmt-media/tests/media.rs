//! `image_generate`, `video_analyze` and `x_search` against mock providers
//! (spec: web-and-media-tools, `image_generate` tool; Built-in image
//! generation providers and models; `video_analyze` tool; `x_search` tool;
//! Media tool credentials and billing are explicit).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::Config;
use pmt_media::video::{DescribeFuture, VideoDescriber};
use pmt_tools::result::Status;
use pmt_tools::usage::BILLING_SOURCE;
use pmt_tools::variants::RouteTraits;
use serde_json::{Value, json};
use support::{Rig, details, media_path, png};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn image_host() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/out.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(png(), "image/png"))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn the_default_provider_and_model_make_a_stored_media_file() {
    let server = image_host().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux-2/klein/9b"))
        .and(header("authorization", "Key FAL_KEY-value"))
        .and(body_partial_json(
            json!({"prompt": "a lighthouse", "image_size": "landscape_16_9"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"images": [{"url": format!("{}/out.png", server.uri())}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let rig = Rig::new(Config::default())
        .secret("FAL_KEY")
        .at("fal", &server.uri());
    let result = rig
        .session()
        .generate_image(&json!({"prompt": "a lighthouse"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let text = result.text_content();
    let file = media_path(&text);
    assert!(file.starts_with(rig.media_dir()), "{text}");
    assert_eq!(std::fs::read(&file).unwrap(), png());
    let shown = details(&text);
    assert_eq!(shown["provider"], "fal");
    assert_eq!(shown["model"], "fal-ai/flux-2/klein/9b");
    assert_eq!(shown["mime"], "image/png");
    assert_eq!(shown["size"], "8x4");
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:fal");
}

#[tokio::test]
async fn a_custom_openai_compatible_endpoint_gets_the_model_verbatim_without_quality() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer OPENAI_API_KEY-value"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"b64_json": pmt_media::http::b64(&png())}]})),
        )
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.image_gen.provider = Some("openai".into());
    config.image_gen.openai.base_url = Some(format!("{}/v1", server.uri()));
    config.image_gen.openai.model = Some("grok-imagine-image".into());
    let rig = Rig::new(config).secret("OPENAI_API_KEY");
    let result = rig
        .session()
        .generate_image(&json!({"prompt": "x", "aspect_ratio": "square"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "grok-imagine-image");
    assert_eq!(body["size"], "1024x1024");
    assert!(body.get("quality").is_none(), "{body}");
}

#[tokio::test]
async fn a_catalog_openai_id_sends_its_model_and_quality() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .and(body_partial_json(
            json!({"model": "gpt-image-2", "quality": "high"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"b64_json": pmt_media::http::b64(&png())}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.image_gen.provider = Some("openai".into());
    config.image_gen.model = Some("gpt-image-2-high".into());
    let rig = Rig::new(config)
        .secret("OPENAI_API_KEY")
        .at("openai", &server.uri());
    let result = rig.session().generate_image(&json!({"prompt": "x"})).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(details(&result.text_content())["requested_quality"], "high");
}

#[tokio::test]
async fn a_subscription_billed_image_records_its_billing_source() {
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer nous-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"images": [{"b64_json": pmt_media::http::b64(&png()), "content_type": "image/png"}]}),
        ))
        .mount(&gateway)
        .await;
    let mut config = Config::default();
    config.image_gen.provider = Some("nous".into());
    config
        .tool_gateway
        .urls
        .insert("image".into(), gateway.uri());
    let rig = Rig::new(config).signed_in("nous");
    let session = rig.session();
    let environments = Arc::new(pmt_media::MediaSessions::default());
    let _attached = environments.attach("ses_img", Arc::new(session));
    let tool = pmt_media::image::tool::registered(&environments);
    let pmt_tools::Executor::Handler(handler) = &tool.executor else {
        panic!("a handler");
    };
    let context = pmt_tools::CallContext {
        call_id: "call_1".into(),
        session_id: "ses_img".into(),
        cancel: pmt_core::cancel::CancelToken::new(),
    };
    let result = handler
        .call(&context, json!({"prompt": "x"}))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(
        result.metadata[BILLING_SOURCE],
        "subscription:nous/personal"
    );
    let ledger = rig.usage.lock().unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].purpose, "tool.image_generate");
    assert_eq!(ledger[0].billing_source, "subscription:nous/personal");
}

fn advertised(config: &Config) -> (String, Value) {
    let sessions = Arc::new(pmt_media::MediaSessions::default());
    let tool = pmt_media::image::tool::registered(&sessions);
    let shown = tool.advertised(config, &RouteTraits::default());
    (shown.description, shown.parameters)
}

#[test]
fn editing_is_advertised_only_when_the_model_edits() {
    let mut config = Config::default();
    config.image_gen.model = Some("fal-ai/z-image/turbo".into());
    let (description, schema) = advertised(&config);
    assert!(schema["properties"].get("image_url").is_none());
    assert!(schema["properties"].get("reference_image_urls").is_none());
    assert!(description.contains("cannot edit"), "{description}");
    config.image_gen.model = Some("fal-ai/nano-banana-pro".into());
    let (description, schema) = advertised(&config);
    assert!(schema["properties"].get("image_url").is_some());
    assert_eq!(schema["properties"]["reference_image_urls"]["maxItems"], 14);
    assert!(
        description.contains("Editing is available"),
        "{description}"
    );
}

#[tokio::test]
async fn image_inputs_to_a_text_only_model_name_an_edit_capable_one() {
    let mut config = Config::default();
    config.image_gen.model = Some("fal-ai/qwen-image".into());
    let rig = Rig::new(config)
        .secret("FAL_KEY")
        .at("fal", "http://127.0.0.1:9");
    let result = rig
        .session()
        .generate_image(&json!({"prompt": "x", "image_url": "https://a.test/i.png"}))
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result
            .text_content()
            .contains("such as fal-ai/flux-2/klein/9b"),
        "{}",
        result.text_content()
    );
}

#[test]
fn parallel_generations_are_bounded_by_tool_concurrency() {
    let mut config = Config::default();
    assert_eq!(pmt_media::image::parallel_limit(&config), 4);
    config.image_gen.max_parallel_requests = 16;
    config.agent.tool_concurrency = 2;
    assert_eq!(pmt_media::image::parallel_limit(&config), 2);
    config.image_gen.max_parallel_requests = 0;
    assert_eq!(pmt_media::image::parallel_limit(&config), 1);
}

#[tokio::test]
async fn an_unknown_image_provider_fails_closed() {
    let mut config = Config::default();
    config.image_gen.provider = Some("midjourney".into());
    let rig = Rig::new(config).secret("FAL_KEY");
    let result = rig.session().generate_image(&json!({"prompt": "x"})).await;
    assert_eq!(
        result.text_content(),
        "image_generate unavailable: image_gen provider 'midjourney' is not installed or not enabled"
    );
}

struct Counting(AtomicUsize);

impl VideoDescriber for Counting {
    fn route(&self) -> (String, String) {
        ("google".into(), "gemini-2.5-flash".into())
    }

    fn billing_source(&self) -> String {
        "api_key:google".into()
    }

    fn describe<'a>(&'a self, video: &'a [u8], mime: &'a str, _q: &'a str) -> DescribeFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let answer = format!("{} bytes of {mime}", video.len());
        Box::pin(async move { Ok(answer) })
    }
}

#[tokio::test]
async fn an_oversized_video_is_refused_before_any_provider_call() {
    let describer = Arc::new(Counting(AtomicUsize::new(0)));
    let mut rig = Rig::new(Config::default());
    rig.video = Some(describer.clone());
    let big = rig.workspace().join("big.mp4");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(70 << 20)
        .unwrap();
    let files = rig.files();
    let result = rig
        .session()
        .analyze_video(Some(&files), "big.mp4", "what happens?")
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("50 MiB limit"),
        "{}",
        result.text_content()
    );
    assert_eq!(describer.0.load(Ordering::SeqCst), 0);
    // Within the limit it reaches the route; above 20 MiB with a warning.
    let medium = rig.workspace().join("medium.webm");
    std::fs::File::create(&medium)
        .unwrap()
        .set_len(21 << 20)
        .unwrap();
    let result = rig
        .session()
        .analyze_video(Some(&files), "medium.webm", "what happens?")
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert!(
        result
            .text_content()
            .starts_with("[warning: the video is 21.0 MiB")
    );
    assert!(result.text_content().contains("of video/webm"));
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:google");
    let other = rig
        .session()
        .analyze_video(Some(&files), "clip.gif", "?")
        .await;
    assert!(other.text_content().contains("unsupported video type"));
}

/// Regression: a local video is read through the file tools' guards,
/// like `vision_analyze`'s images: a path outside the workspace, a
/// symlinked `.mp4` naming a dotenv file and a device-namespace (UNC)
/// path are refused before any provider call.
#[tokio::test]
async fn a_local_video_outside_the_files_guards_is_refused() {
    let describer = Arc::new(Counting(AtomicUsize::new(0)));
    let mut rig = Rig::new(Config::default());
    rig.video = Some(describer.clone());
    let files = rig.files();
    let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let elsewhere = outside.path().join("private.mp4");
    std::fs::write(&elsewhere, b"not yours").unwrap();
    let mut refused = vec![
        elsewhere.to_string_lossy().into_owned(),
        r"\\host\share\a.mp4".to_owned(),
    ];
    #[cfg(unix)]
    {
        let secrets = rig.workspace().join(".env");
        std::fs::write(&secrets, "API_KEY=sk-123").unwrap();
        std::os::unix::fs::symlink(&secrets, rig.workspace().join("x.mp4")).unwrap();
        refused.push("x.mp4".to_owned());
    }
    let session = rig.session();
    for path in &refused {
        let result = session.analyze_video(Some(&files), path, "?").await;
        assert_eq!(
            result.status,
            Status::Error,
            "{path}: {}",
            result.text_content()
        );
    }
    let without = session.analyze_video(None, "clip.mp4", "?").await;
    assert_eq!(without.status, Status::Error);
    assert_eq!(describer.0.load(Ordering::SeqCst), 0, "no provider call");
}

#[test]
fn video_analyze_needs_a_video_capable_route() {
    let sessions = Arc::new(pmt_media::MediaSessions::default());
    let tool = pmt_media::video::registered(&sessions, &Arc::default());
    let check = tool.route_check.as_ref().unwrap();
    let config = Config::default();
    assert_eq!(
        check(&config, &RouteTraits::default()),
        Err("no video-capable route".to_owned())
    );
    let mut route = RouteTraits::default();
    route
        .auxiliary
        .insert(pmt_media::video::VIDEO_AUX.to_owned());
    assert_eq!(check(&config, &route), Ok(()));
}

#[tokio::test]
async fn a_future_x_search_window_is_refused_without_a_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let rig = Rig::new(Config::default())
        .secret("XAI_API_KEY")
        .at("xai", &server.uri());
    let next_week = jiff::Zoned::now()
        .date()
        .checked_add(jiff::Span::new().days(7))
        .unwrap();
    let result = rig
        .session()
        .x_search(&json!({"query": "rust", "from_date": next_week.to_string()}))
        .await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.text_content(),
        format!("x_search: from_date ({next_week}) is in the future")
    );
}

#[tokio::test]
async fn an_uncited_filtered_answer_is_degraded_and_the_key_wins_over_the_sign_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer XAI_API_KEY-value"))
        .and(body_partial_json(json!({"model": "grok-4.5", "tools": [{"type": "x_search", "allowed_x_handles": ["someone"]}]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"output": [{"content": [{"type": "output_text", "text": "Nothing found."}]}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let rig = Rig::new(Config::default())
        .secret("XAI_API_KEY")
        .signed_in("xai-oauth")
        .at("xai", &server.uri());
    let result = rig
        .session()
        .x_search(&json!({"query": "news", "allowed_x_handles": ["@someone"]}))
        .await;
    assert_eq!(result.status, Status::Degraded, "{}", result.text_content());
    let body: Value = serde_json::from_str(&result.text_content()).unwrap();
    assert_eq!(body["degraded"], true);
    assert_eq!(body["answer"], "Nothing found.");
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:xai");
}

#[tokio::test]
async fn x_search_retries_a_server_error_and_uses_the_sign_in_without_a_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer xai-oauth-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"output": [{"content": [
            {"type": "output_text", "text": "Yes.", "annotations": [{"type": "url_citation", "url": "https://x.com/a/status/1"}]}]}]})))
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.x_search.retries = 1;
    let rig = Rig::new(config)
        .signed_in("xai-oauth")
        .at("xai", &server.uri());
    let result = rig
        .session()
        .x_search(&json!({"query": "q", "allowed_x_handles": ["a"]}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let body: Value = serde_json::from_str(&result.text_content()).unwrap();
    assert_eq!(body["citations"], json!(["https://x.com/a/status/1"]));
    assert_eq!(
        result.metadata[BILLING_SOURCE],
        "subscription:xai-oauth/personal"
    );
}
