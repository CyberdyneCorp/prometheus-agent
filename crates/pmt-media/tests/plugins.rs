//! Image, speech and transcription providers of enabled plugins (spec:
//! plugin-system, Provider-object plugins fail closed when unregistered):
//! a selected name that is not built in reaches the provider a plugin
//! registered, its answer is checked and billed as it reports, and a name
//! nothing registered still fails closed.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pmt_config::Config;
use pmt_media::plugin::{Kind, MediaPlugins, PluginFuture};
use pmt_media::speech::audio::Ffmpeg;
use pmt_tools::result::Status;
use pmt_tools::usage::BILLING_SOURCE;
use serde_json::{Value, json};
use support::{Rig, details, media_path, png};

/// Plugin `studio` registering image provider `pics`, speech provider
/// `voicebox` and transcription provider `ears`; records each request.
struct Studio {
    data: PathBuf,
    seen: Mutex<Vec<(Kind, Value)>>,
    /// Where the image answer points.
    image_at: PathBuf,
}

impl Studio {
    fn new(rig: &Rig) -> Arc<Self> {
        let data = rig.profile.join("plugin-data/studio");
        let image_at = data.join("files/out.png");
        std::fs::create_dir_all(image_at.parent().unwrap()).unwrap();
        std::fs::write(&image_at, png()).unwrap();
        Arc::new(Self {
            data,
            seen: Mutex::default(),
            image_at,
        })
    }

    fn answer(&self, kind: Kind, request: &Value) -> Value {
        match kind {
            Kind::ImageGen => json!({"success": true, "image": self.image_at, "model": "pics-1",
                "billing_source": "api_key:pics"}),
            Kind::Tts => {
                let out = request["output_path"].as_str().unwrap();
                assert!(out.starts_with(self.data.to_str().unwrap()), "{out}");
                std::fs::write(out, b"ID3 plugin speech").unwrap();
                json!(out)
            }
            Kind::Stt => {
                let audio = std::fs::read(request["file_path"].as_str().unwrap()).unwrap();
                json!({"success": true, "transcript": format!(" heard {} bytes ", audio.len())})
            }
        }
    }
}

impl MediaPlugins for Studio {
    fn plugin_of(&self, kind: Kind, id: &str) -> Option<String> {
        let registered = matches!(
            (kind, id),
            (Kind::ImageGen, "pics") | (Kind::Tts, "voicebox") | (Kind::Stt, "ears")
        );
        registered.then(|| "studio".to_owned())
    }

    fn invoke<'a>(&'a self, kind: Kind, _: &'a str, request: Value) -> PluginFuture<'a> {
        Box::pin(async move {
            let answer = self.answer(kind, &request);
            self.seen.lock().unwrap().push((kind, request));
            Ok(answer)
        })
    }
}

#[tokio::test]
async fn a_plugin_image_provider_makes_a_stored_media_file_billed_as_reported() {
    let mut config = Config::default();
    config.image_gen.provider = Some("pics".into());
    let mut rig = Rig::new(config);
    let studio = Studio::new(&rig);
    rig.plugins = studio.clone();
    let result = rig
        .session()
        .generate_image(&json!({"prompt": "a lighthouse", "aspect_ratio": "square"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let text = result.text_content();
    let file = media_path(&text);
    assert!(file.starts_with(rig.media_dir()), "{text}");
    assert_eq!(std::fs::read(&file).unwrap(), png());
    let shown = details(&text);
    assert_eq!(shown["provider"], "pics");
    assert_eq!(shown["model"], "pics-1");
    assert_eq!(shown["size"], "8x4");
    assert_eq!(result.metadata[BILLING_SOURCE], "api_key:pics");
    let seen = studio.seen.lock().unwrap();
    assert_eq!(seen[0].1["prompt"], "a lighthouse");
    assert_eq!(seen[0].1["aspect_ratio"], "square");
}

#[tokio::test]
async fn a_plugin_answer_naming_a_file_outside_its_data_directory_is_refused() {
    let mut config = Config::default();
    config.image_gen.provider = Some("pics".into());
    let mut rig = Rig::new(config);
    let studio = Studio::new(&rig);
    let secret = rig.workspace().join("id_rsa.png");
    std::fs::write(&secret, png()).unwrap();
    rig.plugins = Arc::new(Studio {
        data: studio.data.clone(),
        seen: Mutex::default(),
        image_at: secret,
    });
    let result = rig.session().generate_image(&json!({"prompt": "x"})).await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result
            .text_content()
            .contains("outside its plugin's data directory"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn a_plugin_speech_provider_speaks_into_the_audio_cache_billed_locally() {
    let mut config = Config::default();
    config.tts.provider = Some("voicebox".into());
    let mut rig = Rig::new(config);
    let studio = Studio::new(&rig);
    rig.plugins = studio.clone();
    let result = rig
        .session()
        .with_ffmpeg(Ffmpeg::none())
        .text_to_speech(&json!({"text": "Hello there.", "speed": 1.5}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let text = result.text_content();
    let file = media_path(&text);
    assert!(file.starts_with(rig.profile.join("cache/audio")), "{text}");
    assert_eq!(std::fs::read(&file).unwrap(), b"ID3 plugin speech");
    assert_eq!(details(&text)["provider"], "voicebox");
    assert_eq!(result.metadata[BILLING_SOURCE], "local");
    let seen = studio.seen.lock().unwrap();
    assert_eq!(seen[0].1["text"], "Hello there.");
    assert_eq!(seen[0].1["speed"], 1.5);
    assert!(
        !PathBuf::from(seen[0].1["output_path"].as_str().unwrap()).exists(),
        "the exchange file is consumed"
    );
}

#[tokio::test]
async fn a_plugin_transcription_provider_is_used_strictly() {
    let mut config = Config::default();
    config.stt.provider = Some("ears".into());
    let mut rig = Rig::new(config);
    rig.plugins = Studio::new(&rig);
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS1234").unwrap();
    let session = rig.session();
    let transcript = session.transcribe(&clip).await.unwrap();
    assert_eq!(transcript.text, "heard 8 bytes");
    assert_eq!(transcript.provider, "ears");
    assert_eq!(transcript.billing_source, "local");
    assert_eq!(session.stt_choice().unwrap().provider, "ears");
}

#[tokio::test]
async fn names_no_plugin_registered_still_fail_closed() {
    let mut config = Config::default();
    config.image_gen.provider = Some("ghost".into());
    config.tts.provider = Some("ghost".into());
    config.stt.provider = Some("ghost".into());
    let mut rig = Rig::new(config);
    rig.plugins = Studio::new(&rig);
    let session = rig.session();
    let image = session.generate_image(&json!({"prompt": "x"})).await;
    assert!(
        image
            .text_content()
            .contains("image_gen provider 'ghost' is not installed or not enabled"),
        "{}",
        image.text_content()
    );
    let speech = session.text_to_speech(&json!({"text": "x"})).await;
    assert!(
        speech
            .text_content()
            .contains("tts provider 'ghost' is not installed or not enabled"),
        "{}",
        speech.text_content()
    );
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    assert_eq!(
        session.transcribe(&clip).await.unwrap_err(),
        "stt is configured to use ghost, but stt provider 'ghost' is not installed or not enabled"
    );
}
