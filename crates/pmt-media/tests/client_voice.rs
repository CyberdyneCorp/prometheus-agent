//! What a desktop attached to a remote daemon is told about the profile's
//! speech providers (spec: voice-mode, Client-direct voice for remote
//! daemons).

mod support;

use pmt_config::Config;
use pmt_config::schema::CommandSttConfig;
use pmt_media::speech::client::{HOST_ONLY, NOT_DIRECT, Path};
use support::Rig;

#[test]
fn cloud_providers_with_a_key_go_direct() {
    let mut config = Config::default();
    config.stt.provider = Some("openai".into());
    config.tts.provider = Some("openai".into());
    let rig = Rig::new(config)
        .secret("OPENAI_API_KEY")
        .at("openai", "https://speech.example/v1");
    let session = rig.session();
    let stt = session.client_stt(true);
    assert_eq!(stt.path, Path::Direct);
    assert_eq!(stt.provider, "openai");
    assert_eq!(stt.base_url, "https://speech.example/v1");
    assert!(stt.api_key.as_deref().is_some_and(|k| !k.is_empty()));
    let tts = session.client_tts(true);
    assert_eq!(tts.path, Path::Direct);
    assert!(tts.api_key.is_some());
    // Off: everything goes through the daemon, and no key is handed out.
    let relayed = session.client_stt(false);
    assert_eq!(relayed.path, Path::Relay);
    assert_eq!(relayed.reason.as_deref(), Some(NOT_DIRECT));
    assert!(relayed.api_key.is_none());
}

#[test]
fn host_only_providers_relay_through_the_daemon() {
    let mut config = Config::default();
    config.stt.provider = Some("fake".into());
    config.stt.providers.insert(
        "fake".into(),
        CommandSttConfig {
            command: "printf hi".into(),
            ..CommandSttConfig::default()
        },
    );
    config.tts.provider = Some("piper".into());
    let session = Rig::new(config).session();
    let stt = session.client_stt(true);
    assert_eq!(stt.path, Path::Relay);
    assert_eq!(stt.reason.as_deref(), Some(HOST_ONLY));
    assert!(stt.api_key.is_none());
    let tts = session.client_tts(true);
    assert_ne!(tts.path, Path::Direct, "{tts:?}");
    assert!(tts.api_key.is_none());
}
