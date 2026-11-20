//! `video_generate` and the tools video providers contribute, against
//! mock providers (spec: web-and-media-tools, `video_generate` tool;
//! Provider-contributed media tools).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pmt_config::Config;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_media::video_gen::{Sleep, Timer};
use pmt_media::{MediaSession, MediaSessions};
use pmt_tools::probe::ProbeCache;
use pmt_tools::registry::{Builtins, ToolRegistry};
use pmt_tools::result::Status;
use pmt_tools::selection::Selection;
use pmt_tools::surface::{SurfaceInputs, ToolSurface};
use pmt_tools::usage::BILLING_SOURCE;
use pmt_tools::variants::RouteTraits;
use serde_json::{Value, json};
use support::{Rig, details, media_path};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A clock that moves only when slept on.
#[derive(Default)]
struct FakeTimer(AtomicU64);

impl Timer for FakeTimer {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }

    fn sleep(&self, period: Duration) -> Sleep<'_> {
        let millis = u64::try_from(period.as_millis()).unwrap();
        self.0.fetch_add(millis, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// An MP4 header.
fn mp4() -> Vec<u8> {
    b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom".to_vec()
}

fn session(rig: &Rig, timer: &Arc<FakeTimer>) -> MediaSession {
    rig.session()
        .with_timer(Arc::clone(timer) as Arc<dyn Timer>)
}

async fn video_host(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/out.mp4"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(mp4(), "video/mp4"))
        .mount(server)
        .await;
}

fn configured(provider: &str) -> Config {
    let mut config = Config::default();
    config.video_gen.provider = Some(provider.into());
    config
}

#[tokio::test]
async fn a_stuck_job_times_out_with_the_provider_job_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/pixverse/v6/text-to-video"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id": "job-1"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/pixverse/requests/job-1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "IN_QUEUE"})))
        // Every 5 seconds for the default 900.
        .expect(180)
        .mount(&server)
        .await;
    let rig = Rig::new(Config::default())
        .secret("FAL_KEY")
        .at("fal", &server.uri());
    let timer = Arc::new(FakeTimer::default());
    let result = session(&rig, &timer)
        .generate_video(&json!({"prompt": "a lighthouse at dusk"}))
        .await;
    assert_eq!(result.status, Status::Timeout, "{}", result.text_content());
    let report: Value = serde_json::from_str(&result.text_content()).unwrap();
    assert_eq!(report["status"], "timeout");
    assert_eq!(report["job_id"], "job-1");
    assert_eq!(report["provider"], "fal");
    assert_eq!(timer.now(), Duration::from_mins(15));
}

#[tokio::test]
async fn a_finished_job_lands_in_the_blob_store_as_media() {
    let server = MockServer::start().await;
    video_host(&server).await;
    Mock::given(method("POST"))
        .and(path("/fal-ai/veo3.1/image-to-video"))
        .and(header("authorization", "Key FAL_KEY-value"))
        .and(body_partial_json(
            json!({"prompt": "waves", "duration": "8s",
            "aspect_ratio": "16:9", "resolution": "720p", "image_url": "https://a.test/f.png"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id": "j2"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/j2/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "COMPLETED"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fal-ai/veo3.1/requests/j2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"video": {"url": format!("{}/out.mp4", server.uri())}})),
        )
        .mount(&server)
        .await;
    let mut config = configured("fal");
    config.video_gen.model = Some("veo3.1".into());
    let rig = Rig::new(config).secret("FAL_KEY").at("fal", &server.uri());
    let timer = Arc::new(FakeTimer::default());
    let result = session(&rig, &timer)
        .generate_video(
            &json!({"prompt": "waves", "duration": 30, "aspect_ratio": "21:9",
            "image_url": "https://a.test/f.png", "reference_image_urls": ["https://a.test/r.png"]}),
        )
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let text = result.text_content();
    let file = media_path(&text);
    assert!(file.starts_with(rig.media_dir()), "{text}");
    assert_eq!(file.extension().unwrap(), "mp4");
    assert_eq!(std::fs::read(&file).unwrap(), mp4());
    let shown = details(&text);
    assert_eq!(shown["provider"], "fal");
    assert_eq!(shown["model"], "veo3.1");
    assert_eq!(shown["job_id"], "j2");
    assert_eq!(shown["duration"], 8);
    assert_eq!(shown["ignored"], json!(["reference_image_urls"]));
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:fal");
}

#[tokio::test]
async fn openrouter_videos_download_from_the_content_endpoint_with_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/videos"))
        .and(body_partial_json(
            json!({"model": "minimax/hailuo-3-max", "resolution": "768p"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "or-1"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/videos/or-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "completed"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/videos/or-1/content"))
        .and(header("authorization", "Bearer OPENROUTER_API_KEY-value"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(mp4(), "video/mp4"))
        .expect(1)
        .mount(&server)
        .await;
    let rig = Rig::new(configured("openrouter"))
        .secret("OPENROUTER_API_KEY")
        .at("openrouter", &server.uri());
    let timer = Arc::new(FakeTimer::default());
    let result = session(&rig, &timer)
        .generate_video(&json!({"prompt": "a city", "resolution": "1080p"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(details(&result.text_content())["resolution"], "768p");
}

#[tokio::test]
async fn a_failed_job_names_the_providers_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/videos/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id": "x1"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/videos/x1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "failed", "error": {"message": "content policy"}})),
        )
        .mount(&server)
        .await;
    let rig = Rig::new(configured("xai"))
        .secret("XAI_API_KEY")
        .at("xai", &server.uri());
    let timer = Arc::new(FakeTimer::default());
    let result = session(&rig, &timer)
        .generate_video(&json!({"prompt": "x"}))
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("content policy"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn an_unknown_video_provider_fails_closed() {
    let rig = Rig::new(configured("sora")).secret("FAL_KEY");
    let result = rig.session().generate_video(&json!({"prompt": "x"})).await;
    assert_eq!(
        result.text_content(),
        "video_generate unavailable: video_gen provider 'sora' is not installed or not enabled"
    );
}

fn advertised(config: &Config) -> Value {
    let sessions = Arc::new(MediaSessions::default());
    let tool = pmt_media::video_gen::tool::registered(&sessions);
    tool.advertised(config, &RouteTraits::default()).parameters
}

#[test]
fn the_schema_follows_the_configured_model() {
    let mut config = configured("fal");
    config.video_gen.model = Some("veo3.1".into());
    let schema = advertised(&config);
    let properties = &schema["properties"];
    assert_eq!(properties["duration"]["minimum"], 4);
    assert_eq!(properties["duration"]["maximum"], 8);
    assert_eq!(properties["aspect_ratio"]["enum"], json!(["16:9", "9:16"]));
    assert_eq!(properties["resolution"]["enum"], json!(["720p", "1080p"]));
    assert!(properties.get("negative_prompt").is_some());
    assert!(properties.get("reference_image_urls").is_none());
    let schema = advertised(&configured("xai"));
    let properties = &schema["properties"];
    assert_eq!(properties["reference_image_urls"]["maxItems"], 7);
    for gone in ["negative_prompt", "audio", "seed", "upscale"] {
        assert!(properties.get(gone).is_none(), "{gone}");
    }
}

/// The names a session with toolset `video_gen` exposes.
async fn exposed(config: &Config, secrets: &[&str]) -> Vec<String> {
    let environments = Arc::new(pmt_tools::environments::Environments::default());
    let sessions = Arc::new(MediaSessions::default());
    let tools = pmt_media::tools(&sessions, &environments);
    let present: Vec<String> = secrets.iter().map(|s| (*s).to_owned()).collect();
    let registry = ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()))
        .with_secret_presence(Arc::new(move |name: &str| {
            present.iter().any(|s| s == name)
        }));
    let selection = Selection::resolve(&registry, &["video_gen".to_owned()], &[]).unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
        route: &RouteTraits::default(),
    })
    .await;
    surface
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect()
}

#[tokio::test]
async fn provider_tools_are_absent_for_another_provider() {
    let names = exposed(&configured("fal"), &["FAL_KEY", "XAI_API_KEY"]).await;
    assert_eq!(names, ["video_generate"]);
    let names = exposed(&configured("xai"), &["XAI_API_KEY"]).await;
    assert_eq!(
        names,
        ["video_generate", "xai_video_edit", "xai_video_extend"]
    );
    // xAI configured without a credential: nothing to call.
    assert!(exposed(&configured("xai"), &[]).await.is_empty());
}

#[tokio::test]
async fn edit_refuses_a_url_that_is_not_an_xai_result_without_contacting_xai() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id": "e"})))
        .expect(0)
        .mount(&server)
        .await;
    let rig = Rig::new(configured("xai"))
        .secret("XAI_API_KEY")
        .at("xai", &server.uri());
    for url in [
        "https://example.com/clip.mp4",
        "http://vidgen.x.ai/clip.mp4",
        "/home/me/clip.mp4",
    ] {
        let result = rig
            .session()
            .xai_video(
                pmt_media::video_gen::xai::Operation::Edit,
                &json!({"prompt": "make it night", "video_url": url}),
            )
            .await;
        assert_eq!(result.status, Status::Error, "{url}");
        assert!(result.text_content().contains("public HTTPS MP4"), "{url}");
    }
}

#[tokio::test]
async fn extend_sends_the_xai_video_and_stores_the_result() {
    let server = MockServer::start().await;
    video_host(&server).await;
    let source = "https://vidgen.x.ai/abc/video.mp4";
    Mock::given(method("POST"))
        .and(path("/videos/extensions"))
        .and(header("authorization", "Bearer XAI_API_KEY-value"))
        .and(body_partial_json(
            json!({"model": "grok-imagine-video", "prompt": "the fox runs",
            "video": {"url": source}, "duration": 10}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id": "ext-1"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/videos/ext-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"status": "done", "video": {"url": format!("{}/out.mp4", server.uri())}}),
        ))
        .mount(&server)
        .await;
    let rig = Rig::new(configured("xai"))
        .secret("XAI_API_KEY")
        .at("xai", &server.uri());
    let timer = Arc::new(FakeTimer::default());
    let result = session(&rig, &timer)
        .xai_video(
            pmt_media::video_gen::xai::Operation::Extend,
            &json!({"prompt": "the fox runs", "video_url": source, "duration": 30}),
        )
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let shown = details(&result.text_content());
    assert_eq!(shown["job_id"], "ext-1");
    assert_eq!(shown["video_url"], format!("{}/out.mp4", server.uri()));
    assert_eq!(
        std::fs::read(media_path(&result.text_content())).unwrap(),
        mp4()
    );
}
