//! Voice mode's use of the speech providers against mock providers (spec:
//! voice-mode, Long recordings and provider upload limits; Sentence-
//! streamed spoken replies).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::Config;
use pmt_media::speech::audio::{Ffmpeg, wav_from_pcm};
use pmt_media::speech::tts::Speak;
use serde_json::json;
use support::Rig;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Answers `part <n>` to the n-th upload.
struct Parts(AtomicUsize);

impl Respond for Parts {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        ResponseTemplate::new(200).set_body_json(json!({ "text": format!(" part {n} ") }))
    }
}

/// The length in seconds of the WAV file inside a multipart upload.
fn uploaded_seconds(body: &[u8]) -> f64 {
    let at = body.windows(4).position(|w| w == b"RIFF").unwrap();
    let wav = &body[at..];
    let rate = u32::from_le_bytes(wav[24..28].try_into().unwrap());
    let data = u32::from_le_bytes(wav[40..44].try_into().unwrap());
    f64::from(data) / 2.0 / f64::from(rate)
}

/// A two-hour voice note (at 100 samples a second, so the test stays
/// small) said without a break except two-second pauses at 2,800 s and
/// 5,600 s, against Mistral's 3,600 s limit: it goes up in three pieces
/// cut in those pauses, and the agent gets one transcript.
#[tokio::test]
async fn a_long_voice_note_is_split_at_pauses_and_transcribed_as_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Parts(AtomicUsize::new(0)))
        .expect(3)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.stt.provider = Some("mistral".into());
    let rig = Rig::new(config)
        .secret("MISTRAL_API_KEY")
        .at("mistral", &server.uri());
    let rate = 100;
    let pcm: Vec<u8> = (0..7_200 * rate)
        .flat_map(|i: u32| {
            let second = i / rate;
            let quiet = (2_800..2_802).contains(&second) || (5_600..5_602).contains(&second);
            let sample: i16 = if quiet {
                4
            } else if i.is_multiple_of(2) {
                3_000
            } else {
                -3_000
            };
            sample.to_le_bytes()
        })
        .collect();
    let note = rig.workspace().join("long note.wav");
    std::fs::write(&note, wav_from_pcm(&pcm, rate)).unwrap();
    let session = rig.session().with_ffmpeg(Ffmpeg::none());
    let transcript = session.transcribe(&note).await.unwrap();
    assert_eq!(transcript.text, "part 1 part 2 part 3");
    let uploads: Vec<f64> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| uploaded_seconds(&r.body))
        .collect();
    assert!(uploads.iter().all(|s| *s <= 3_600.0), "{uploads:?}");
    assert!(
        (uploads.iter().sum::<f64>() - 7_200.0).abs() < 0.1,
        "{uploads:?}"
    );
    assert!((2_800.0..2_802.0).contains(&uploads[0]), "{uploads:?}");
    assert!(
        (5_600.0..5_602.0).contains(&(uploads[0] + uploads[1])),
        "{uploads:?}"
    );
}

/// A compressed recording over the limit cannot be cut without ffmpeg:
/// it is refused before any upload, naming the limit.
#[tokio::test]
async fn an_over_limit_compressed_note_without_ffmpeg_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "x"})))
        .expect(0)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.stt.provider = Some("groq".into());
    let rig = Rig::new(config)
        .secret("GROQ_API_KEY")
        .at("groq", &server.uri());
    let note = rig.workspace().join("big.ogg");
    let mut bytes = b"OggS".to_vec();
    bytes.resize(26 << 20, 0);
    std::fs::write(&note, bytes).unwrap();
    let session = rig.session().with_ffmpeg(Ffmpeg::none());
    assert_eq!(
        session.transcribe(&note).await.unwrap_err(),
        "stt is configured to use groq, but the audio is 26 MiB, over groq's 25 MiB upload limit; ffmpeg is needed to re-encode or split it"
    );
}

/// `openai` streams raw PCM: the chunks reach the player as they arrive.
#[tokio::test]
async fn openai_speech_streams_raw_pcm() {
    let server = MockServer::start().await;
    let pcm: Vec<u8> = (0..4_800i16).flat_map(i16::to_le_bytes).collect();
    Mock::given(method("POST"))
        .and(path("/audio/speech"))
        .and(header("authorization", "Bearer OPENAI_API_KEY-value"))
        .and(body_partial_json(
            json!({"response_format": "pcm", "input": "Hello there."}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pcm.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = Config::default();
    config.tts.provider = Some("openai".into());
    let rig = Rig::new(config)
        .secret("OPENAI_API_KEY")
        .at("openai", &server.uri());
    let session = rig.session();
    let (rate, mut chunks) = session
        .speak_streaming("Hello there.", &Speak::default())
        .await
        .expect("openai streams")
        .unwrap();
    assert_eq!(rate, 24_000);
    let mut got = Vec::new();
    while let Some(chunk) = chunks.recv().await {
        got.extend(chunk);
    }
    assert_eq!(got, pcm);
    // The keyless default synthesizes whole files instead.
    let rig = Rig::new(Config::default());
    assert!(
        rig.session()
            .speak_streaming("Hi.", &Speak::default())
            .await
            .is_none()
    );
}
