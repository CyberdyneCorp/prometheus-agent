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

/// Serves one read-aloud connection: records each SSML message and
/// answers MP3 audio frames then `turn.end`.
async fn serve_edge<S>(stream: S, seen: Arc<Mutex<Vec<String>>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
}

/// A plain read-aloud server.
async fn edge_server(seen: Arc<Mutex<Vec<String>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            pmt_core::spawn::spawn(serve_edge(stream, Arc::clone(&seen)));
        }
    });
    format!("ws://{address}/readaloud")
}

/// A read-aloud server speaking TLS as `speech.test` with a certificate
/// of a private CA: its loopback port and the CA's PEM.
async fn private_ca_edge_server(seen: Arc<Mutex<Vec<String>>>) -> (u16, String) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["speech.test".into()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![leaf.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    pmt_core::spawn::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, seen) = (acceptor.clone(), Arc::clone(&seen));
            pmt_core::spawn::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    serve_edge(tls, seen).await;
                }
            });
        }
    });
    (port, ca.pem())
}

/// An HTTP proxy that records each `CONNECT` target and tunnels it to
/// `127.0.0.1:<port>` (so `speech.test` needs no DNS): its URL.
async fn connect_proxy(port: u16, targets: Arc<Mutex<Vec<String>>>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    pmt_core::spawn::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let targets = Arc::clone(&targets);
            pmt_core::spawn::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut words = head.split_whitespace();
                if words.next() != Some("CONNECT") {
                    let _ = client.write_all(b"HTTP/1.1 405 Only CONNECT\r\n\r\n").await;
                    return;
                }
                targets
                    .lock()
                    .unwrap()
                    .push(words.next().unwrap_or_default().to_owned());
                let mut upstream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    format!("http://{address}")
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
async fn edge_speech_goes_through_the_network_proxy_and_trusts_the_extra_ca() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (port, ca_pem) = private_ca_edge_server(Arc::clone(&seen)).await;
    let targets = Arc::new(Mutex::new(Vec::new()));
    let proxy = connect_proxy(port, Arc::clone(&targets)).await;
    let mut config = Config::default();
    config.network.proxy = Some(proxy);
    let mut rig = Rig::new(config).at("edge", &format!("wss://speech.test:{port}/readaloud"));
    let speak = |rig: &Rig| {
        let session = rig.session();
        async move {
            session
                .text_to_speech(&json!({"text": "Through the proxy."}))
                .await
        }
    };

    // Without network.extra_ca_file the private CA is not trusted.
    let refused = speak(&rig).await;
    assert_eq!(refused.status, Status::Error, "{}", refused.text_content());
    assert!(
        refused.text_content().contains("TLS"),
        "{}",
        refused.text_content()
    );
    assert!(seen.lock().unwrap().is_empty());

    let ca_file = rig.profile.join("corp-ca.pem");
    std::fs::write(&ca_file, ca_pem).unwrap();
    rig.config.network.extra_ca_file = Some(ca_file.display().to_string());
    let result = speak(&rig).await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(
        std::fs::read(media_path(&result.text_content())).unwrap(),
        b"ID3edge-mp3"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    // Both connections were tunnelled; none went direct.
    let expected = format!("speech.test:{port}");
    assert_eq!(*targets.lock().unwrap(), vec![expected.clone(), expected]);
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

/// A link inside the workspace does not carry the write outside it.
#[cfg(unix)]
#[tokio::test]
async fn an_output_path_escaping_through_a_symlink_is_refused() {
    let rig = Rig::new(Config::default());
    let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::os::unix::fs::symlink(outside.path(), rig.workspace().join("link")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("speech.mp3"),
        rig.workspace().join("speech.mp3"),
    )
    .unwrap();
    let session = rig.session();
    for requested in ["link/speech.mp3", "link/new/dir/speech.mp3", "speech.mp3"] {
        let result = session
            .text_to_speech(&json!({"text": "x", "output_path": requested}))
            .await;
        assert_eq!(result.status, Status::Error, "{requested}");
        assert!(
            result.text_content().contains("outside the workspace"),
            "{requested}: {}",
            result.text_content()
        );
    }
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
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

/// `pre_transcription` callbacks answering in registration order.
struct Rewriting(Vec<Value>);

impl pmt_media::plugin::MediaPlugins for Rewriting {
    fn plugin_of(&self, _: pmt_media::plugin::Kind, _: &str) -> Option<String> {
        None
    }

    fn invoke<'a>(
        &'a self,
        kind: pmt_media::plugin::Kind,
        id: &'a str,
        _: Value,
    ) -> pmt_media::plugin::PluginFuture<'a> {
        Box::pin(async move { Err(pmt_media::plugin::not_installed(kind, id)) })
    }

    fn pre_transcription(
        &self,
        mut request: pmt_media::plugin::SttRequest,
    ) -> pmt_media::plugin::SttFuture<'_> {
        Box::pin(async move {
            for answer in &self.0 {
                request.apply(answer);
            }
            request
        })
    }
}

/// `pre_transcription` (spec: plugin-system, Streaming and lifecycle
/// observer hooks): fired after the provider is resolved, its `prompt`,
/// `language` and `model` reach the request (last writer wins) and every
/// other field is ignored.
#[tokio::test]
async fn pre_transcription_changes_reach_the_stt_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "ola"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.stt.provider = Some("groq".into());
    let mut rig = Rig::new(config)
        .secret("GROQ_API_KEY")
        .at("groq", &server.uri());
    rig.plugins = Arc::new(Rewriting(vec![
        json!({"language": "es", "model": "whisper-x", "provider": "openai"}),
        json!({"language": "pt", "prompt": "Lisboa, Porto"}),
    ]));
    let clip = rig.workspace().join("note.ogg");
    std::fs::write(&clip, b"OggS").unwrap();
    let transcript = rig
        .session()
        .with_ffmpeg(Ffmpeg::none())
        .transcribe(&clip)
        .await
        .unwrap();
    assert_eq!(
        (transcript.provider.as_str(), transcript.model.as_str()),
        ("groq", "whisper-x"),
        "the provider never moves"
    );
    let body = &server.received_requests().await.unwrap()[0].body;
    let text = String::from_utf8_lossy(body);
    assert!(text.contains("name=\"language\"\r\n\r\npt"), "{text}");
    assert!(text.contains("name=\"model\"\r\n\r\nwhisper-x"), "{text}");
    assert!(
        text.contains("name=\"prompt\"\r\n\r\nLisboa, Porto"),
        "{text}"
    );
}
