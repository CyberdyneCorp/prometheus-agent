//! Client-direct voice for remote daemons (spec: voice-mode, Client-direct
//! voice for remote daemons): what a desktop attached to a remote daemon
//! needs to talk to the profile's cloud speech providers itself, so its
//! recordings never cross the daemon link. A provider that runs only on
//! the daemon host (an on-device engine, a command or plugin provider) or
//! signs in through the daemon (the subscription gateway, an OAuth
//! credential) stays on the relay path instead, and says why.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::stt::builtin_listener;
use super::tts;
use crate::MediaSession;

/// How the client reaches one speech provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// The client calls the provider itself with the credential given.
    Direct,
    /// The client sends the audio (or text) to the daemon, which calls
    /// the provider.
    Relay,
    /// No provider resolves.
    None,
}

/// One speech provider as the client sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRoute {
    /// Direct, relay or none.
    pub path: Path,
    /// The provider id.
    pub provider: String,
    /// Its model.
    pub model: String,
    /// The API base URL (direct only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// The credential (direct only; the client keeps it in memory for the
    /// voice session only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The voice (speech).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub voice: String,
    /// The spoken language (transcription).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Why the relay path (or none) is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The request shape the client speaks (direct only): `openai` (the
    /// OpenAI transcription and speech APIs and their compatible
    /// endpoints), `elevenlabs`, `gemini` or `minimax`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wire: String,
    /// Extra request fields the configuration adds (direct only): form
    /// fields of a transcription, JSON body fields of a speech request.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
    /// Drop `[bracketed]` audio tags from the text before speaking it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strip_tags: bool,
}

impl ClientRoute {
    fn relay(provider: &str, model: &str, reason: &str) -> Self {
        Self {
            path: Path::Relay,
            provider: provider.to_owned(),
            model: model.to_owned(),
            base_url: String::new(),
            api_key: None,
            voice: String::new(),
            language: None,
            reason: Some(reason.to_owned()),
            wire: String::new(),
            extra: Map::new(),
            strip_tags: false,
        }
    }

    fn none(reason: String) -> Self {
        Self {
            path: Path::None,
            provider: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_key: None,
            voice: String::new(),
            language: None,
            reason: Some(reason),
            wire: String::new(),
            extra: Map::new(),
            strip_tags: false,
        }
    }
}

/// The request shape of a speech-to-text provider.
fn stt_wire(provider: &str) -> &'static str {
    if provider == "elevenlabs" {
        "elevenlabs"
    } else {
        "openai"
    }
}

/// The request shape of a text-to-speech provider (`openai` for the
/// OpenAI speech API and its compatible endpoints).
fn tts_wire(provider: &str) -> &'static str {
    match provider {
        "elevenlabs" => "elevenlabs",
        "gemini" => "gemini",
        "minimax" => "minimax",
        _ => "openai",
    }
}

/// The reason a Gemini voice framed by a persona prompt file relays.
pub const PERSONA_FILE: &str = "the persona prompt file is on the daemon host";

/// Whether a provider with this billing source signs in (a subscription,
/// the gateway, OAuth) rather than holding an API key: it cannot go
/// direct.
fn signed_in(billing_source: &str) -> bool {
    !billing_source.starts_with("api_key")
}

/// The reason a host-only provider relays.
pub const HOST_ONLY: &str = "the provider runs on the daemon host only";
/// The reason a signed-in provider relays.
pub const SIGNED_IN: &str = "the provider signs in through the daemon";
/// The reason everything relays when `voice.client_direct = false`.
pub const NOT_DIRECT: &str = "voice.client_direct is off";

impl MediaSession {
    /// The speech-to-text provider for a client; `direct` is
    /// `voice.client_direct`.
    #[must_use]
    pub fn client_stt(&self, direct: bool) -> ClientRoute {
        let choice = match self.stt_choice() {
            Ok(choice) => choice,
            Err(why) => return ClientRoute::none(why),
        };
        let relay = |reason| ClientRoute::relay(&choice.provider, &choice.model, reason);
        if !direct {
            return relay(NOT_DIRECT);
        }
        let Some(listener) = builtin_listener(self.setup()).filter(|l| l.access.token.is_some())
        else {
            return relay(HOST_ONLY);
        };
        if signed_in(&listener.access.billing_source) || listener.provider == "nous" {
            return relay(SIGNED_IN);
        }
        ClientRoute {
            path: Path::Direct,
            provider: listener.provider.clone(),
            model: listener.model.clone(),
            base_url: listener.base_url.clone(),
            api_key: listener
                .access
                .token
                .as_ref()
                .map(|t| t.expose().to_owned()),
            voice: String::new(),
            language: listener.language.clone(),
            reason: None,
            wire: stt_wire(&listener.provider).to_owned(),
            extra: self.stt_extra(&listener.provider),
            strip_tags: false,
        }
    }

    /// The form fields a transcription adds for `provider`, as the daemon
    /// sends them.
    fn stt_extra(&self, provider: &str) -> Map<String, Value> {
        let mut extra = Map::new();
        if provider == "elevenlabs" {
            let config = &self.setup().config().stt.elevenlabs;
            extra.insert("diarize".into(), json!(config.diarize.to_string()));
            extra.insert(
                "tag_audio_events".into(),
                json!(config.tag_audio_events.to_string()),
            );
        }
        extra
    }

    /// The JSON body fields a speech request adds for `provider`, as the
    /// daemon sends them.
    fn tts_extra(&self, provider: &str) -> Map<String, Value> {
        let tts = &self.setup().config().tts;
        let mut extra = Map::new();
        match provider {
            "openai" => {
                if let Some(speed) = tts.openai.speed {
                    extra.insert("speed".into(), json!(speed));
                }
            }
            "xai" => {
                extra.insert("speed".into(), json!(tts.xai.speed));
                extra.insert("language".into(), json!(tts.xai.language));
            }
            _ => {}
        }
        extra
    }

    /// The text-to-speech provider for a client; `direct` is
    /// `voice.client_direct`.
    #[must_use]
    pub fn client_tts(&self, direct: bool) -> ClientRoute {
        let voice = match tts::resolve(self.setup()) {
            Ok(voice) => voice,
            Err(why) => return ClientRoute::none(why),
        };
        let relay = |reason| {
            let mut route = ClientRoute::relay(&voice.provider, &voice.model, reason);
            route.voice.clone_from(&voice.voice);
            route
        };
        if !direct {
            return relay(NOT_DIRECT);
        }
        let host_only = tts::LOCAL.contains(&voice.provider.as_str())
            || voice.command.is_some()
            || voice.plugin.is_some()
            || voice.access.token.is_none();
        if host_only {
            return relay(HOST_ONLY);
        }
        if signed_in(&voice.access.billing_source) || voice.provider == "nous" {
            return relay(SIGNED_IN);
        }
        let gemini = &self.setup().config().tts.gemini;
        if voice.provider == "gemini" && gemini.persona_prompt_file.is_some() {
            return relay(PERSONA_FILE);
        }
        ClientRoute {
            path: Path::Direct,
            provider: voice.provider.clone(),
            model: voice.model.clone(),
            base_url: voice.base_url.clone(),
            api_key: voice.access.token.as_ref().map(|t| t.expose().to_owned()),
            voice: voice.voice.clone(),
            language: None,
            reason: None,
            wire: tts_wire(&voice.provider).to_owned(),
            extra: self.tts_extra(&voice.provider),
            strip_tags: voice.provider == "gemini" && !gemini.audio_tags,
        }
    }
}
