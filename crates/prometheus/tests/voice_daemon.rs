//! The daemon's side of client-direct voice (spec: voice-mode,
//! Client-direct voice for remote daemons): how a desktop is told to reach
//! the profile's speech providers, and the relay transcription.

#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::{ok, run, run_with_stdin, temp_home};
use pmt_rpc::contracts::{
    VoiceClientConfigGet, VoiceClientConfigParams, VoiceTranscribe, VoiceTranscribeParams,
};

fn home_with(config: &str) -> common::TestHome {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    let path = home.path().join("profiles/default/config.toml");
    let text = std::fs::read_to_string(&path).unwrap() + config;
    std::fs::write(&path, text).unwrap();
    home
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_only_provider_relays_and_the_recording_does_not_stay() {
    let home = home_with(
        "\n[stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
         command = \"head -c 4 {input_path} | grep -q RIFF && printf 'relayed words'\"\n",
    );
    let _daemon = common::daemon::start(home.path(), &[]);
    let (client, _inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    let config = client
        .call_typed::<VoiceClientConfigGet>(&VoiceClientConfigParams {
            profile: "default".to_owned(),
            credentials: true,
        })
        .await
        .unwrap();
    assert!(config.client_direct);
    assert_eq!(config.stt.path, "relay");
    assert_eq!(
        config.stt.reason.as_deref(),
        Some("the provider runs on the daemon host only")
    );
    assert!(config.stt.api_key.is_none());
    let wav = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pmt-voice/tests/fixtures/question.wav"),
    )
    .unwrap();
    let heard = client
        .call_typed::<VoiceTranscribe>(&VoiceTranscribeParams {
            profile: "default".to_owned(),
            session_id: None,
            audio: base64::engine::general_purpose::STANDARD.encode(wav),
        })
        .await
        .unwrap();
    assert_eq!(heard.text, "relayed words");
    let voice = home.path().join("profiles/default/cache/voice");
    let left = std::fs::read_dir(&voice).map_or(0, Iterator::count);
    assert_eq!(left, 0, "no recording stays under cache/voice");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_provider_with_a_key_is_handed_to_the_client() {
    let home = home_with("\n[stt]\nprovider = \"groq\"\n\n[tts]\nprovider = \"openai\"\n");
    let set = |name: &str| {
        ok(&run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            "sk-test-key\n",
        ));
    };
    set("GROQ_API_KEY");
    set("OPENAI_API_KEY");
    let _daemon = common::daemon::start(home.path(), &[]);
    let (client, _inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    let config = client
        .call_typed::<VoiceClientConfigGet>(&VoiceClientConfigParams {
            profile: "default".to_owned(),
            credentials: true,
        })
        .await
        .unwrap();
    assert_eq!(config.stt.path, "direct");
    assert_eq!(config.stt.provider, "groq");
    assert_eq!(config.stt.api_key.as_deref(), Some("sk-test-key"));
    assert_eq!(config.tts.path, "direct");
    assert_eq!(config.tts.provider, "openai");
    assert_eq!(config.chat_mode, "chained");
    // The desktop speaks each provider's request shape and listens with
    // the profile's `[voice]` settings.
    assert_eq!(config.stt.wire, "openai");
    assert_eq!(config.tts.wire, "openai");
    assert!(config.listen.barge_in);
    assert_eq!(config.listen.silence_duration_ms, 3_000);
    // Without asking for credentials (a refresh, a realtime session) no
    // key leaves the daemon.
    let keyless = client
        .call_typed::<VoiceClientConfigGet>(&VoiceClientConfigParams {
            profile: "default".to_owned(),
            credentials: false,
        })
        .await
        .unwrap();
    assert_eq!(keyless.stt.path, "direct");
    assert!(keyless.stt.api_key.is_none() && keyless.tts.api_key.is_none());
}
