//! Chat controls of the profile's platforms (spec: messaging-gateway,
//! Text approval and busy-mode commands; Adapter operations from chat):
//!
//! - `/busy [queue|steer|interrupt|status]`: a mode is written to
//!   `gateway.platforms.<name>.busy.input_mode` in the profile's
//!   `config.toml` through the validated config writer, so it applies to
//!   the next inbound message and survives restarts; `status` names the
//!   effective mode and where it comes from;
//! - `/platform list|pause <name>|resume <name>` (admin tier; `list` is
//!   `/platforms`);
//! - `/voice [on|tts|off|status]` (spec: voice-mode, Gateway per-chat
//!   voice reply command): the chat's spoken-reply mode, stored with the
//!   chat so it survives restarts and overrides `gateway.voice.reply_mode`.

use pmt_config::schema::{BusyInputMode, VoiceReplyMode};

use super::Lane;
use crate::messaging::key::Routing;
use crate::messaging::operate;

/// The name of a busy mode as configured.
fn mode_name(mode: BusyInputMode) -> &'static str {
    match mode {
        BusyInputMode::Queue => "queue",
        BusyInputMode::Steer => "steer",
        BusyInputMode::Interrupt => "interrupt",
    }
}

/// How a chat's `/voice` choice is stored.
fn voice_key(mode: VoiceReplyMode) -> &'static str {
    match mode {
        VoiceReplyMode::Off => "off",
        VoiceReplyMode::Mirror => "mirror",
        VoiceReplyMode::Always => "always",
    }
}

fn voice_mode(key: &str) -> Option<VoiceReplyMode> {
    match key {
        "off" => Some(VoiceReplyMode::Off),
        "mirror" => Some(VoiceReplyMode::Mirror),
        "always" => Some(VoiceReplyMode::Always),
        _ => None,
    }
}

/// The source [`Lane::chat_voice`] names for a mode stored with the chat.
const CHAT_SOURCE: &str = "chat";

/// What `/voice status` calls a mode.
fn voice_label(strings: &pmt_i18n::Strings, mode: VoiceReplyMode) -> String {
    let key = match mode {
        VoiceReplyMode::Off => "gateway.voice_off",
        VoiceReplyMode::Mirror => "gateway.voice_mirror",
        VoiceReplyMode::Always => "gateway.voice_always",
    };
    strings.get(key).to_owned()
}

/// `on` or `off` in the chat's language.
fn on_off(strings: &pmt_i18n::Strings, on: bool) -> String {
    strings
        .get(if on { "gateway.on" } else { "gateway.off" })
        .to_owned()
}

impl Lane {
    /// `/platform list | pause <name> | resume <name>`.
    pub(super) async fn platform_command(&self, routing: &Routing, args: &str) {
        let words: Vec<&str> = args.split_whitespace().collect();
        let shared = &self.shared;
        let strings = self.strings();
        let reply = match words.as_slice() {
            [] | ["list"] => operate::listing(shared, &self.profile),
            ["pause", name] => {
                let actor = format!("{}:{}", routing.platform, routing.sender_id);
                match operate::pause(shared, &self.profile, &name.to_lowercase(), &actor) {
                    Ok(true) => pmt_i18n::t!(strings, "gateway.platform_paused_now", name = name),
                    Ok(false) => {
                        pmt_i18n::t!(strings, "gateway.platform_already_paused", name = name)
                    }
                    Err(err) => err.message().to_owned(),
                }
            }
            ["resume", name] => {
                match operate::resume(shared, &self.profile, &name.to_lowercase()).await {
                    Ok(status) => {
                        let line = operate::platform_line(&strings, &status);
                        pmt_i18n::t!(strings, "gateway.platform_resumed", line = line)
                    }
                    Err(err) => err.message().to_owned(),
                }
            }
            _ => pmt_i18n::t!(strings, "gateway.platform_usage"),
        };
        self.say(&reply);
    }

    /// `/footer [on|off|status]`: the chat's runtime footer, stored with
    /// the chat (the display policy's `runtime_footer` applies until set).
    pub(super) fn footer_command(&self, routing: &Routing, args: &str) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let strings = self.strings();
        let chosen = match args.trim() {
            "on" => Some(true),
            "off" => Some(false),
            "" | "status" => {
                let stored = store
                    .chat_prefs(&routing.platform, &routing.chat_id)
                    .unwrap_or_default()
                    .footer;
                let on = stored.unwrap_or_else(|| self.display().runtime_footer);
                let source = strings.get(if stored.is_some() {
                    "gateway.source_chat"
                } else {
                    "gateway.source_policy"
                });
                let state = on_off(&strings, on);
                self.say(&pmt_i18n::t!(
                    strings,
                    "gateway.footer_status",
                    state = state,
                    source = source
                ));
                return;
            }
            _ => {
                self.say(strings.get("gateway.footer_usage"));
                return;
            }
        };
        match store.set_chat_footer(&routing.platform, &routing.chat_id, chosen) {
            Ok(()) => {
                let state = on_off(&strings, chosen == Some(true));
                self.say(&pmt_i18n::t!(strings, "gateway.footer_set", state = state));
            }
            Err(err) => self.say(err.message()),
        }
    }

    /// `/voice [on|tts|off|status]`: `on` speaks replies to voice
    /// messages, `tts` speaks every reply, `off` keeps replies text.
    pub(super) fn voice_command(&self, routing: &Routing, args: &str) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let strings = self.strings();
        let chosen = match args.trim() {
            "on" => VoiceReplyMode::Mirror,
            "tts" => VoiceReplyMode::Always,
            "off" => VoiceReplyMode::Off,
            "" | "status" => {
                let (mode, source) = self.chat_voice(routing);
                let source = if source == CHAT_SOURCE {
                    strings.get("gateway.source_chat")
                } else {
                    source
                };
                let mode = voice_label(&strings, mode);
                self.say(&pmt_i18n::t!(
                    strings,
                    "gateway.voice_status",
                    mode = mode,
                    source = source
                ));
                return;
            }
            _ => {
                self.say(strings.get("gateway.voice_usage"));
                return;
            }
        };
        match store.set_chat_voice(&routing.platform, &routing.chat_id, Some(voice_key(chosen))) {
            Ok(()) => {
                let mode = voice_label(&strings, chosen);
                self.say(&pmt_i18n::t!(strings, "gateway.voice_set", mode = mode));
            }
            Err(err) => self.say(err.message()),
        }
    }

    /// The chat's spoken-reply mode and where it comes from.
    pub(super) fn chat_voice(&self, routing: &Routing) -> (VoiceReplyMode, &'static str) {
        let stored = self
            .shared
            .store(&self.profile)
            .and_then(|store| store.chat_prefs(&routing.platform, &routing.chat_id).ok())
            .and_then(|prefs| prefs.voice)
            .and_then(|key| voice_mode(&key));
        match stored {
            Some(mode) => (mode, CHAT_SOURCE),
            None => (
                self.config().map_or(VoiceReplyMode::Mirror, |s| {
                    s.config().gateway.voice.reply_mode
                }),
                "gateway.voice.reply_mode",
            ),
        }
    }

    /// `/busy [queue|steer|interrupt|status]`.
    pub(super) fn busy_command(&self, routing: &Routing, args: &str) {
        match args.trim() {
            "" | "status" => self.busy_status(routing),
            mode @ ("queue" | "steer" | "interrupt") => self.set_busy(routing, mode),
            _ => self.say(self.strings().get("gateway.busy_usage")),
        }
    }

    fn busy_status(&self, routing: &Routing) {
        let Some(scope) = self.config() else {
            return;
        };
        let config = scope.config();
        let platform = config.gateway.platform(&routing.platform);
        let reply = match platform.busy.input_mode {
            Some(mode) => pmt_i18n::t!(
                self.strings(),
                "gateway.busy_platform",
                mode = mode_name(mode)
            ),
            None => format!("{} (busy.input_mode)", mode_name(config.busy.input_mode)),
        };
        self.say(&reply);
    }

    fn set_busy(&self, routing: &Routing, mode: &str) {
        let Some(scope) = self.config() else {
            return;
        };
        let key = format!("gateway.platforms.{}.busy.input_mode", routing.platform);
        let value = serde_json::to_string(mode).expect("a string serializes");
        match scope.profile().config_file().set(&key, &value) {
            Ok(()) => {
                tracing::info!(
                    profile = self.profile,
                    platform = routing.platform,
                    mode,
                    "busy mode set"
                );
                self.say(&pmt_i18n::t!(
                    self.strings(),
                    "gateway.busy_set",
                    platform = routing.platform,
                    mode = mode
                ));
            }
            Err(err) => {
                let text =
                    pmt_i18n::t!(self.strings(), "gateway.busy_not_set", why = err.message());
                self.say(&text);
            }
        }
    }
}
