//! `vision_analyze` and image input routing against mock routes (spec:
//! web-and-media-tools, `vision_analyze` tool; Image input routing;
//! agent-turn-loop, Image input routing). The `vision` auxiliary route is
//! the built-in `mock` provider answering through `AuxCaller`.

use std::collections::BTreeMap;

use base64::Engine as _;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::{ImageFormat, Rgba, RgbaImage};
use pmt_agent::vision::{AuxDescriber, image_part};
use pmt_config::schema::{Config, ImageInputMode};
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_providers::ProviderRegistry;
use pmt_providers::auxiliary::{AuxCaller, AuxRoute, AuxSource, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::mock::{Mock, Script};
use pmt_providers::request::Part;
use pmt_providers::resolve::{NoExtras, Resolver, Target, TcpProbe, Trace};
use pmt_tools::result::{Content, Status};
use pmt_tools::vision::tool::AnalyzeArgs;
use pmt_tools::vision::{
    Delivery, DescribeFuture, Describer, ImageData, RoutingInputs, VisionSession, VisionSetup,
};
use wiremock::matchers::path;
use wiremock::{Mock as Http, MockServer, ResponseTemplate};

const ANSWER: &str = "A red square on a white page.";

/// The `vision` auxiliary route on the mock provider.
async fn mock_describer() -> AuxDescriber {
    let config = Config::default();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    let secrets: BTreeMap<String, SecretString> = BTreeMap::new();
    let resolver = Resolver {
        registry: &registry,
        config: &config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &TcpProbe {
            timeout: Duration::from_millis(200),
        },
        ad_hoc: None,
    };
    let target = Target {
        provider: "mock".into(),
        model: Some("vision-model".into()),
        ..Target::default()
    };
    let route = resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap();
    let aux = AuxRoute {
        task: AuxTask::Vision,
        route,
        source: AuxSource::Config,
        timeout: Duration::from_secs(5),
        retries: 0,
    };
    let script = Script::parse(
        &format!("[[rule]]\ntext = \"{ANSWER}\"\nrepeat = true\n"),
        "vision.toml",
    )
    .unwrap();
    let client = pmt_net::client(&OutboundPolicy::default()).unwrap();
    let caller = AuxCaller::new(client).with_mock(Arc::new(Mock::new(script)));
    AuxDescriber::new(
        Arc::new(caller),
        aux,
        CallContext::new(SessionId::new(), "auxiliary.vision"),
    )
}

/// Records the image it was asked about.
#[derive(Default)]
struct Recording(Mutex<Option<ImageData>>);

impl Describer for Recording {
    fn describe<'a>(&'a self, image: &'a ImageData, _: &'a str) -> DescribeFuture<'a> {
        *self.0.lock().unwrap() = Some(image.clone());
        Box::pin(async { Ok("described".to_owned()) })
    }
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut image = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));
    // A marker at the crop's far corner.
    image.put_pixel(
        799.min(width - 1),
        199.min(height - 1),
        Rgba([255, 0, 0, 255]),
    );
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn data_url(bytes: &[u8], mime: &str) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn temp_dir() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

fn fetcher(allow_private: bool) -> Arc<Fetcher> {
    let policy = DestinationPolicy::new(allow_private, &[]).unwrap();
    Arc::new(Fetcher::new(&OutboundPolicy::default(), policy).unwrap())
}

fn session(
    main_vision: bool,
    tool_result_images: bool,
    describer: Option<Arc<dyn Describer>>,
) -> VisionSession {
    VisionSession::new(VisionSetup {
        routing: RoutingInputs {
            mode: ImageInputMode::Auto,
            main_vision,
            aux_explicit: false,
        },
        tool_result_images,
        describer,
        fetcher: fetcher(true),
        media_dir: temp_dir(),
    })
}

fn args(image_url: String, region: Option<[u32; 4]>) -> AnalyzeArgs {
    AnalyzeArgs {
        image_url,
        question: "What is in the corner?".into(),
        region,
    }
}

#[tokio::test]
async fn a_text_only_main_model_gets_the_auxiliary_description() {
    let vision = session(false, true, Some(Arc::new(mock_describer().await)));
    let result = vision
        .analyze(None, &args(data_url(&png(64, 64), "image/png"), None))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(result.text_content(), ANSWER);
    assert!(
        result
            .content
            .iter()
            .all(|c| matches!(c, Content::Text { .. }))
    );
}

#[tokio::test]
async fn a_region_crop_keeps_the_original_resolution() {
    let screenshot = data_url(&png(4_000, 3_000), "image/png");
    // Native: the crop itself is the result.
    let native = session(true, true, None);
    let result = native
        .analyze(None, &args(screenshot.clone(), Some([0, 0, 800, 200])))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let Some(Content::Media { mime, path }) = result.content.get(1) else {
        panic!("no image part: {:?}", result.content);
    };
    assert_eq!(mime, "image/png");
    let crop = image::open(path).unwrap().to_rgba8();
    assert_eq!(crop.dimensions(), (800, 200));
    assert_eq!(crop.get_pixel(799, 199), &Rgba([255, 0, 0, 255]));
    // Described: the auxiliary route sees the same full-resolution crop.
    let recording = Arc::new(Recording::default());
    let described = session(false, false, Some(recording.clone()));
    described
        .analyze(None, &args(screenshot, Some([0, 0, 800, 200])))
        .await;
    let seen = recording.0.lock().unwrap().clone().unwrap();
    assert_eq!(seen.dimensions().unwrap(), (800, 200));
}

#[tokio::test]
async fn without_a_native_path_or_a_vision_route_the_tool_is_unavailable() {
    let vision = session(false, true, None);
    assert_eq!(vision.unavailable(), Some("no vision-capable route"));
    let result = vision
        .analyze(None, &args(data_url(&png(8, 8), "image/png"), None))
        .await;
    assert_eq!(
        result.text_content(),
        "vision_analyze unavailable: no vision-capable route"
    );
}

#[tokio::test]
async fn an_image_already_attached_to_the_user_message_is_not_sent_again() {
    let vision = session(true, true, None);
    vision.set_current_attachments(vec!["photo.png".into()]);
    let result = vision.analyze(None, &args("photo.png".into(), None)).await;
    assert!(
        result.text_content().contains("already in context"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn long_animations_are_refused() {
    use image::codecs::gif::GifEncoder;
    let mut bytes = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        let frames = (0..101).map(|_| image::Frame::new(RgbaImage::new(2, 2)));
        encoder.encode_frames(frames).unwrap();
    }
    let vision = session(true, true, None);
    let result = vision
        .analyze(None, &args(data_url(&bytes, "image/gif"), None))
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("more than 100 frames"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn downloads_pass_the_url_policy() {
    let server = MockServer::start().await;
    Http::given(path("/shot.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png(32, 16)))
        .mount(&server)
        .await;
    let vision = session(true, true, None);
    let result = vision
        .analyze(None, &args(format!("{}/shot.png", server.uri()), None))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert!(result.text_content().contains("\"width\":32"));
    // A cloud metadata address is refused even when private ones are allowed.
    let blocked = vision
        .analyze(
            None,
            &args("http://169.254.169.254/latest/meta-data".into(), None),
        )
        .await;
    assert_eq!(blocked.status, Status::Error);
    assert!(
        blocked.text_content().contains("metadata"),
        "{}",
        blocked.text_content()
    );
}

#[tokio::test]
async fn text_mode_keeps_pixels_out_of_the_request() {
    let delivery = RoutingInputs {
        mode: ImageInputMode::Text,
        main_vision: true,
        aux_explicit: false,
    }
    .delivery();
    assert_eq!(delivery, Delivery::Text);
    let describer = mock_describer().await;
    let photo = ImageData::sniff(png(64, 48)).unwrap();
    let part = image_part(photo, "photo.png", delivery, Some(&describer)).await;
    let Part::Text { text } = part else {
        panic!("an image part reached the request: {part:?}");
    };
    assert_eq!(text, format!("[image photo.png: {ANSWER}]"));
}

#[tokio::test]
async fn a_text_only_model_gets_a_description_of_an_attachment() {
    let delivery = RoutingInputs {
        mode: ImageInputMode::Auto,
        main_vision: false,
        aux_explicit: false,
    }
    .delivery();
    let describer = mock_describer().await;
    let shot = ImageData::sniff(png(16, 16)).unwrap();
    let part = image_part(shot, "screenshot.png", delivery, Some(&describer)).await;
    assert!(
        matches!(part, Part::Text { ref text } if text.contains(ANSWER)),
        "{part:?}"
    );
}

const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"><rect width="120" height="60" fill="red"/></svg>"#;

#[tokio::test]
async fn an_svg_on_a_raster_only_route_is_sent_as_png() {
    let svg = ImageData::sniff(SVG.as_bytes().to_vec()).unwrap();
    assert_eq!(svg.mime, "image/svg+xml");
    let part = image_part(svg, "logo.svg", Delivery::Native, None).await;
    let Part::Image {
        media_type, data, ..
    } = part
    else {
        panic!("not an image part: {part:?}");
    };
    assert_eq!(media_type, "image/png");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    let raster = image::load_from_memory(&bytes).unwrap();
    assert_eq!((raster.width(), raster.height()), (120, 60));
    // Something that is no image at all becomes a note, never a failure.
    let broken = ImageData {
        mime: "image/svg+xml".into(),
        bytes: b"<svg".to_vec(),
    };
    let note = image_part(broken, "broken.svg", Delivery::Native, None).await;
    assert!(matches!(note, Part::Text { ref text } if text.contains("broken.svg")));
}
