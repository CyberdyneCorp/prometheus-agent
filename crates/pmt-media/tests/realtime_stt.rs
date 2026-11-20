//! Live streaming transcription against a mock realtime transcription
//! socket (spec: voice-mode, Live streaming transcription).

mod support;

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_config::Config;
use serde_json::{Value, json};
use support::Rig;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// What the mock socket does once audio arrives.
#[derive(Clone, Copy)]
enum Script {
    /// A delta per append, the transcript after the commit.
    Answer,
    /// Drops the connection after `n` appends.
    DropAfter(usize),
}

/// A realtime socket at `ws://127.0.0.1:<port>/v1/realtime`; records the
/// session update and the request's authorization header.
async fn mock(script: Script) -> (String, Arc<Mutex<Vec<Value>>>, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let auth = Arc::new(Mutex::new(String::new()));
    let (seen_by, auth_by) = (Arc::clone(&seen), Arc::clone(&auth));
    pmt_core::spawn::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        #[allow(clippy::result_large_err)] // the handshake callback type
        let callback = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                        response| {
            *auth_by.lock().unwrap() = format!(
                "{} {}",
                request.uri(),
                request.headers()["authorization"].to_str().unwrap()
            );
            Ok(response)
        };
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
            .await
            .unwrap();
        let mut appends = 0;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let event: Value = serde_json::from_str(&text).unwrap();
            seen_by.lock().unwrap().push(event.clone());
            match (event["type"].as_str().unwrap(), script) {
                ("input_audio_buffer.append", Script::DropAfter(n)) => {
                    appends += 1;
                    if appends >= n {
                        return;
                    }
                }
                ("input_audio_buffer.append", Script::Answer) => {
                    appends += 1;
                    if appends == 2 {
                        let delta = json!({"type": "conversation.item.input_audio_transcription.delta", "delta": "Turn on"});
                        socket.send(Message::text(delta.to_string())).await.unwrap();
                    }
                }
                ("input_audio_buffer.commit", _) => {
                    let done = json!({"type": "conversation.item.input_audio_transcription.completed", "transcript": "Turn on the lights."});
                    socket.send(Message::text(done.to_string())).await.unwrap();
                }
                _ => {}
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), seen, auth)
}

fn rig(base: &str) -> Rig {
    let mut config = Config::default();
    config.stt.provider = Some("openai".into());
    config.stt.language = "pt".into();
    Rig::new(config).secret("OPENAI_API_KEY").at("openai", base)
}

#[tokio::test]
async fn openai_streams_partial_text_and_the_final_transcript() {
    let (base, seen, auth) = mock(Script::Answer).await;
    let rig = rig(&base);
    let session = rig.session();
    let streaming = session.stt_streaming().expect("openai streams");
    let partials = Arc::new(Mutex::new(Vec::new()));
    let shown = Arc::clone(&partials);
    let mut live = streaming
        .open(Arc::new(move |text: &str| {
            shown.lock().unwrap().push(text.to_owned());
        }))
        .await
        .unwrap();
    for _ in 0..3 {
        live.push(&[100; 320]).unwrap();
    }
    assert_eq!(live.finish().await.unwrap(), "Turn on the lights.");
    assert_eq!(*partials.lock().unwrap(), ["Turn on"]);
    let auth = auth.lock().unwrap().clone();
    assert!(
        auth.starts_with("/v1/realtime?intent=transcription Bearer "),
        "{auth}"
    );
    let seen = seen.lock().unwrap();
    let session = &seen[0]["session"]["audio"]["input"];
    assert_eq!(session["format"]["rate"], 24_000);
    assert_eq!(session["transcription"]["model"], "gpt-4o-transcribe");
    assert_eq!(session["transcription"]["language"], "pt");
    // 320 samples at 16 kHz go out as 480 at 24 kHz.
    let audio = seen[1]["audio"].as_str().unwrap();
    assert_eq!(audio.len(), 480 * 2 * 4 / 3);
}

#[tokio::test]
async fn a_dropped_socket_is_reported_to_the_caller() {
    let (base, _, _) = mock(Script::DropAfter(2)).await;
    let rig = rig(&base);
    let streaming = rig.session().stt_streaming().unwrap();
    let mut live = streaming.open(Arc::new(|_: &str| {})).await.unwrap();
    let mut broke = false;
    for _ in 0..200 {
        if live.push(&[100; 320]).is_err() {
            broke = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(broke, "pushing into a closed session fails");
    assert!(live.finish().await.is_err());
}

#[tokio::test]
async fn providers_without_a_socket_do_not_stream() {
    let mut config = Config::default();
    config.stt.provider = Some("groq".into());
    let rig = Rig::new(config).secret("GROQ_API_KEY");
    assert!(rig.session().stt_streaming().is_none());
}
