//! Client-direct voice for remote daemons (spec: voice-mode, Client-direct
//! voice for remote daemons): `voice.client_config` tells a desktop how to
//! reach the profile's speech providers (direct for cloud providers with
//! an API key, through the daemon otherwise), and `voice.transcribe` is
//! the relay path for recordings.

use base64::Engine as _;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_media::speech::client::{ClientRoute, Path};
use pmt_rpc::contracts::{
    ClientSpeech, VoiceClientConfig, VoiceListening, VoiceTranscribeParams, VoiceTranscript,
};
use serde_json::Value;

fn speech(route: ClientRoute) -> ClientSpeech {
    ClientSpeech {
        path: match route.path {
            Path::Direct => "direct",
            Path::Relay => "relay",
            Path::None => "none",
        }
        .to_owned(),
        provider: route.provider,
        model: route.model,
        base_url: route.base_url,
        api_key: route.api_key,
        voice: route.voice,
        language: route.language,
        reason: route.reason,
        wire: route.wire,
        extra: route.extra,
        strip_tags: route.strip_tags,
    }
}

/// The `[voice]` settings the client listens with.
fn listening(voice: &pmt_config::schema::VoiceModeConfig) -> VoiceListening {
    VoiceListening {
        silence_threshold: voice.silence_threshold,
        speech_confirm_ms: voice.speech_confirm_ms,
        silence_duration_ms: voice.silence_duration_ms,
        no_speech_timeout_secs: voice.no_speech_timeout_secs,
        max_recording_secs: voice.max_recording_secs,
        stop_phrases: voice.stop_phrases.clone(),
        barge_in: voice.barge_in,
        barge_in_threshold_multiplier: voice.barge_in_threshold_multiplier,
    }
}

/// `voice.client_config` of `scope`; direct providers' keys only when
/// `credentials` asks for them.
pub(super) fn config(scope: &ProfileScope, credentials: bool) -> VoiceClientConfig {
    let voice = &scope.config().voice;
    let none = |why: &str| ClientSpeech {
        path: "none".to_owned(),
        provider: String::new(),
        model: String::new(),
        base_url: String::new(),
        api_key: None,
        voice: String::new(),
        language: None,
        reason: Some(why.to_owned()),
        wire: String::new(),
        extra: serde_json::Map::new(),
        strip_tags: false,
    };
    let media = crate::chat::voice::media(scope, "gui", scope.dir());
    let (stt, tts) = media.as_ref().map_or_else(
        || {
            let why = "the profile cannot reach its speech providers";
            (none(why), none(why))
        },
        |media| {
            (
                speech(media.client_stt(voice.client_direct)),
                speech(media.client_tts(voice.client_direct)),
            )
        },
    );
    let keep = |mut speech: ClientSpeech| {
        if !credentials {
            speech.api_key = None;
        }
        speech
    };
    VoiceClientConfig {
        client_direct: voice.client_direct,
        chat_mode: match voice.chat_mode {
            pmt_config::schema::VoiceChatMode::Chained => "chained",
            pmt_config::schema::VoiceChatMode::Realtime => "realtime",
        }
        .to_owned(),
        stt: keep(stt),
        tts: keep(tts),
        listen: listening(voice),
    }
}

/// `voice.transcribe`: the recording is written to the profile's
/// `cache/voice/` (owner-only), transcribed and deleted (unless
/// `voice.keep_recordings`).
pub(super) async fn transcribe(
    scope: &ProfileScope,
    params: VoiceTranscribeParams,
) -> Result<Value, Error> {
    let refused = |why: String| Error::new(&codes::RPC_NOT_SERVED, why);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(params.audio.as_bytes())
        .map_err(|e| {
            Error::new(
                &codes::RPC_INVALID_PARAMS,
                format!("audio is not base64: {e}"),
            )
        })?;
    let media = crate::chat::voice::media(scope, "gui", scope.dir())
        .ok_or_else(|| refused("the profile cannot reach its speech providers".to_owned()))?;
    let ext = pmt_media::speech::audio::Format::sniff(&bytes).extension();
    let dir = crate::chat::voice::voice_dir(scope.dir());
    let path = dir.join(format!("relay-{}.{ext}", ulid::Ulid::generate()));
    crate::chat::voice::write_private(&path, &bytes)
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    let heard = media.transcribe(&path).await;
    if !scope.config().voice.keep_recordings {
        let _ = std::fs::remove_file(&path);
    }
    let transcript = heard.map_err(refused)?;
    if let Some(session) = &params.session_id {
        let served = (
            transcript.provider.as_str(),
            transcript.model.as_str(),
            transcript.billing_source.as_str(),
        );
        media.bill(session, "transcribe", served);
    }
    serde_json::to_value(VoiceTranscript {
        text: transcript.text,
        provider: transcript.provider,
    })
    .map_err(|e| Error::new(&codes::RPC_INTERNAL, e.to_string()))
}
