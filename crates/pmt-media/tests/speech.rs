//! Speech against mock providers (spec: web-and-media-tools,
//! `text_to_speech` tool and delivery; Built-in text-to-speech providers;
//! Built-in speech-to-text providers and resolution; Command-template
//! speech providers).

mod support;

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::Config;
use pmt_config::schema::{CommandSttConfig, CommandTtsConfig};
use pmt_media::speech::audio::Ffmpeg;
use pmt_media::speech::local::Launcher;
use pmt_tools::result::Status;
use pmt_tools::usage::BILLING_SOURCE;
use serde_json::{Value, json};
use support::{Rig, details, media_path};
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A read-aloud server: records each SSML message and answers MP3 audio
/// frames then `turn.end`.
async fn edge_server(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    if !text.contains("Path:ssml") {
                        continue;
                    }
                    seen.lock().unwrap().push(text.to_string());
                    let header = b"Path:audio\r\n";
                    let mut frame = u16::try_from(header.len()).unwrap().to_be_bytes().to_vec();
                    frame.extend_from_slice(header);
                    frame.extend_from_slice(b"ID3edge-mp3");
                    socket.send(Message::binary(frame)).await.unwrap();
                    socket
                        .send(Message::text("Path:turn.end\r\n\r\n{}"))
                        .await
                        .unwrap();
                }
            });
        }
    });
    format!("ws://{address}/readaloud")
}

#[tokio::test]
async fn zero_config_speech_uses_edge_and_aria() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = edge_server(Arc::clone(&seen)).await;
    let rig = Rig::new(Config::default()).at("edge", &url);
    let result = rig
        .session()
        .text_to_speech(&json!({"text": "Hello there."}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let text = result.text_content();
    let file = media_path(&text);
    assert!(file.starts_with(rig.profile.join("cache/audio")), "{text}");
    assert_eq!(std::fs::read(&file).unwrap(), b"ID3edge-mp3");
    assert_eq!(details(&text)["provider"], "edge");
    assert_eq!(details(&text)["voice_compatible"], false);
    assert!(seen.lock().unwrap()[0].contains("<voice name='en-US-AriaNeural'>"));
    assert_eq!(result.metadata[BILLING_SOURCE], "keyless:edge");
}

#[tokio::test]
async fn long_text_goes_out_in_ordered_chunks_and_comes_back_as_one_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/speech"))
        .and(header("authorization", "Bearer OPENAI_API_KEY-value"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let first = body["input"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned();
            ResponseTemplate::new(200).set_body_raw(format!("ID3[{first}]"), "audio/mpeg")
        })
        .expect(3)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.tts.provider = Some("openai".into());
    let rig = Rig::new(config)
        .secret("OPENAI_API_KEY")
        .at("openai", &server.uri());
    let words: Vec<String> = (0..1_700).map(|i| format!("w{i:04}.")).collect();
    let text = words.join(" ");
    assert!(text.len() >= 10_000);
    let result = rig
        .session()
        .text_to_speech(&json!({"text": &text[..10_000]}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let audio =
        String::from_utf8(std::fs::read(media_path(&result.text_content())).unwrap()).unwrap();
    let requests = server.received_requests().await.unwrap();
    let firsts: Vec<String> = requests
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["input"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(firsts[0], "w0000.");
    assert_eq!(
        audio,
        format!("ID3[{}]ID3[{}]ID3[{}]", firsts[0], firsts[1], firsts[2])
    );
    assert_eq!(details(&result.text_content())["requests"], 3);
}

#[cfg(unix)]
#[tokio::test]
async fn a_telegram_session_gets_an_opus_voice_note_from_mp3_speech() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = edge_server(seen).await;
    let mut rig = Rig::new(Config::default()).at("edge", &url);
    rig.surface = "telegram".into();
    // A stand-in ffmpeg that writes an Ogg Opus header to its last argument.
    let ffmpeg = support::script(
        &rig.workspace(),
        "ffmpeg",
        r#"for last; do :; done; printf 'OggS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0OpusHead' > "$last""#,
    );
    let session = rig.session().with_ffmpeg(Ffmpeg::at(ffmpeg));
    let result = session
        .text_to_speech(&json!({"text": "Voice bubble."}))
        .await;
    let text = result.text_content();
    assert!(
        text.lines().next().unwrap().ends_with(" [[as_voice]]"),
        "{text}"
    );
    let file = media_path(&text);
    assert_eq!(file.extension().unwrap(), "ogg");
    assert!(pmt_media::speech::audio::is_opus(
        &std::fs::read(&file).unwrap()
    ));
    assert_eq!(details(&text)["voice_compatible"], true);
    // Without ffmpeg the MP3 stays a plain audio file.
    let plain = rig.session().with_ffmpeg(Ffmpeg::none());
    let text = plain
        .text_to_speech(&json!({"text": "No bubble."}))
        .await
        .text_content();
    assert_eq!(details(&text)["voice_compatible"], false);
}

#[tokio::test]
async fn output_paths_outside_the_workspace_are_refused() {
    let rig = Rig::new(Config::default());
    let result = rig
        .session()
        .text_to_speech(&json!({"text": "x", "output_path": "../../elsewhere.mp3"}))
        .await;
    assert_eq!(result.status, Status::Error);
    assert!(result.text_content().contains("outside the workspace"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_command_provider_receives_paths_with_spaces_as_one_argument() {
    let mut rig = Rig::new(Config::default());
    let engine = support::script(
        &rig.workspace(),
        "mytts",
        r#"[ "$#" -eq 4 ] || { echo "got $# args" >&2; exit 9; }; printf 'ID3' > "$4"; cat "$2" >> "$4""#,
    );
    rig.config.tts.provider = Some("mine".into());
    rig.config.tts.providers.insert(
        "mine".into(),
        CommandTtsConfig {
            command: format!(
                "{} --in {{input_path}} --out {{output_path}}",
                engine.display()
            ),
            ..CommandTtsConfig::default()
        },
    );
    let result = rig
        .session()
        .text_to_speech(&json!({"text": "spoken words"}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    let audio = std::fs::read(media_path(&result.text_content())).unwrap();
    assert_eq!(audio, b"ID3spoken words");
    assert_eq!(result.metadata[BILLING_SOURCE], "local");
}

#[tokio::test]
async fn an_explicit_stt_provider_is_strict() {
    let mut config = Config::default();
    config.stt.provider = Some("groq".into());
    let mut rig = Rig::new(config);
    rig.local = Some(Arc::new(Never));
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    let err = rig.session().transcribe(&clip).await.unwrap_err();
    assert_eq!(
        err,
        "stt is configured to use groq, but GROQ_API_KEY is not set"
    );
}

/// A launcher that fails the test when anything is started.
struct Never;

impl Launcher for Never {
    fn available(&self, _: &str) -> bool {
        true
    }

    fn worker(&self, engine: &str, _: &str, _: &str) -> Result<tokio::process::Command, String> {
        panic!("{engine} must not start");
    }
}

#[cfg(unix)]
struct Recording(std::path::PathBuf);

#[cfg(unix)]
impl Launcher for Recording {
    fn available(&self, engine: &str) -> bool {
        engine == "whisper"
    }

    fn worker(&self, _: &str, _: &str, _: &str) -> Result<tokio::process::Command, String> {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            &format!(
                r#"while read -r line; do echo "$line" >> '{}'; echo '{{"ok":true,"text":"hello from whisper"}}'; done"#,
                self.0.display()
            ),
        ]);
        Ok(command)
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_unset_stt_provider_picks_local_whisper_first() {
    let mut rig = Rig::new(Config::default()).secret("OPENAI_API_KEY");
    let log = rig.workspace().join("requests.jsonl");
    rig.local = Some(Arc::new(Recording(log.clone())));
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    let transcript = rig.session().transcribe(&clip).await.unwrap();
    assert_eq!(transcript.text, "hello from whisper");
    assert_eq!(transcript.provider, "local");
    assert_eq!(transcript.model, "base");
    let request: Value = serde_json::from_str(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["model"], "base");
    assert_eq!(request["language"], "en");
}

#[tokio::test]
async fn nothing_available_is_reported_and_a_cloud_provider_is_chosen_by_key() {
    let rig = Rig::new(Config::default());
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    assert_eq!(
        rig.session().transcribe(&clip).await.unwrap_err(),
        "no STT provider available"
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(header("authorization", "Bearer GROQ_API_KEY-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": " via groq "})))
        .expect(1)
        .mount(&server)
        .await;
    let rig = Rig::new(Config::default())
        .secret("GROQ_API_KEY")
        .at("groq", &server.uri());
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    let session = rig.session().with_ffmpeg(Ffmpeg::none());
    let transcript = session.transcribe(&clip).await.unwrap();
    assert_eq!(transcript.text, "via groq");
    assert_eq!(transcript.provider, "groq");
    assert_eq!(transcript.model, "whisper-large-v3-turbo");
    let body = &server.received_requests().await.unwrap()[0].body;
    let text = String::from_utf8_lossy(body);
    assert!(text.contains("name=\"model\"\r\n\r\nwhisper-large-v3-turbo"));
    assert!(text.contains("filename=\"note.ogg\""));
}

#[cfg(unix)]
#[tokio::test]
async fn a_built_in_stt_name_wins_over_a_same_named_command() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "built-in"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut rig = Rig::new(Config::default())
        .secret("OPENAI_API_KEY")
        .at("openai", &server.uri());
    let marker = rig.workspace().join("command-ran");
    rig.config.stt.provider = Some("openai".into());
    rig.config.stt.providers.insert(
        "openai".into(),
        CommandSttConfig {
            command: format!("touch {} {{input_path}}", marker.display()),
            ..CommandSttConfig::default()
        },
    );
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    let transcript = rig
        .session()
        .with_ffmpeg(Ffmpeg::none())
        .transcribe(&clip)
        .await
        .unwrap();
    assert_eq!(transcript.text, "built-in");
    assert!(!marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_command_stt_reads_its_output_file_then_stdout() {
    let mut rig = Rig::new(Config::default());
    rig.config.stt.provider = Some("mine".into());
    rig.config.stt.providers.insert(
        "mine".into(),
        CommandSttConfig {
            command: "printf 'from file' > {output_path}".into(),
            ..CommandSttConfig::default()
        },
    );
    let clip = rig.workspace().join("my note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    assert_eq!(
        rig.session().transcribe(&clip).await.unwrap().text,
        "from file"
    );
    rig.config.stt.providers.get_mut("mine").unwrap().command = "echo 'from stdout'".into();
    assert_eq!(
        rig.session().transcribe(&clip).await.unwrap().text,
        "from stdout"
    );
    rig.config.stt.providers.get_mut("mine").unwrap().command = "true".into();
    assert_eq!(
        rig.session().transcribe(&clip).await.unwrap_err(),
        "stt is configured to use mine, but command STT provider wrote no output file and produced no stdout"
    );
}
