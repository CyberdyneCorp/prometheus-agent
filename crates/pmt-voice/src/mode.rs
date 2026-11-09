//! `/voice` on the interactive surfaces (spec: voice-mode, Voice mode
//! toggle and commands): `on`, `off`, `tts` (spoken replies), `status`,
//! and bare `/voice` to toggle. Voice mode is the surface's own state: it
//! never touches the session's system prompt, toolset or memory.

use pmt_config::keys::Chord;
use pmt_config::schema::VoiceModeConfig;

/// The refusal when no input device is found.
pub const NO_DEVICE: &str = "No audio input device found";

/// The refusal when no speech-to-text provider resolves.
pub const NO_STT: &str = "speech-to-text is not configured; run `prometheus tools` (Voice)";

/// A `/voice` subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Bare `/voice`.
    Toggle,
    /// `/voice on`.
    On,
    /// `/voice off`.
    Off,
    /// `/voice tts`: spoken replies on or off.
    Tts,
    /// `/voice status`.
    Status,
}

impl Command {
    /// Parses the arguments of `/voice`.
    ///
    /// # Errors
    ///
    /// The usage line.
    pub fn parse(args: &str) -> Result<Self, String> {
        Ok(match args.trim() {
            "" => Self::Toggle,
            "on" => Self::On,
            "off" => Self::Off,
            "tts" => Self::Tts,
            "status" => Self::Status,
            _ => return Err("Usage: /voice [on | off | tts | status]".to_owned()),
        })
    }

    /// Whether running it needs [`Facts`] (the device and providers).
    #[must_use]
    pub fn needs_facts(self, mode: &VoiceMode) -> bool {
        match self {
            Self::On | Self::Status => true,
            Self::Toggle => !mode.on,
            Self::Off | Self::Tts => false,
        }
    }
}

/// What voice mode depends on, probed when it is turned on or reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The input device recordings use.
    pub device: Option<String>,
    /// The speech-to-text provider, or why none resolves.
    pub stt: Result<String, String>,
    /// The text-to-speech provider, or why none resolves.
    pub tts: Result<String, String>,
}

/// One surface's voice mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceMode {
    /// Voice mode is on: the record key records.
    pub on: bool,
    /// Replies are spoken.
    pub spoken: bool,
}

/// The record key as `/voice status` and hints show it (`ctrl+b`).
#[must_use]
pub fn record_key(config: &VoiceModeConfig) -> String {
    Chord::parse(&config.record_key.0).map_or_else(
        |_| config.record_key.0.to_ascii_lowercase(),
        |chord| chord.to_string().to_ascii_lowercase(),
    )
}

impl VoiceMode {
    /// Runs `command`; `facts` must be `Some` when
    /// [`Command::needs_facts`] says so. Returns the line to show.
    #[must_use]
    pub fn run(
        &mut self,
        command: Command,
        facts: Option<&Facts>,
        config: &VoiceModeConfig,
    ) -> String {
        match command {
            Command::Toggle if self.on => self.off(),
            Command::Toggle | Command::On => match facts {
                Some(facts) => self.turn_on(facts, config),
                None => NO_DEVICE.to_owned(),
            },
            Command::Off => self.off(),
            Command::Tts => {
                self.spoken = !self.spoken;
                format!("spoken replies: {}", on_off(self.spoken))
            }
            Command::Status => facts.map_or_else(String::new, |f| self.status(f, config)),
        }
    }

    fn turn_on(&mut self, facts: &Facts, config: &VoiceModeConfig) -> String {
        if facts.device.is_none() {
            return NO_DEVICE.to_owned();
        }
        if facts.stt.is_err() {
            return NO_STT.to_owned();
        }
        if !self.on && config.auto_tts {
            self.spoken = true;
        }
        self.on = true;
        format!(
            "Voice mode on: press {} to talk (spoken replies {}).",
            record_key(config),
            on_off(self.spoken)
        )
    }

    fn off(&mut self) -> String {
        self.on = false;
        "Voice mode off.".to_owned()
    }

    /// The `/voice status` report.
    #[must_use]
    pub fn status(&self, facts: &Facts, config: &VoiceModeConfig) -> String {
        let provider = |p: &Result<String, String>| match p {
            Ok(name) => name.clone(),
            Err(why) => format!("unavailable ({why})"),
        };
        [
            format!("voice: {}", on_off(self.on)),
            format!("spoken replies: {}", on_off(self.spoken)),
            format!("record key: {}", record_key(config)),
            format!(
                "input device: {}",
                facts.device.as_deref().unwrap_or("none found")
            ),
            format!("stt: {}", provider(&facts.stt)),
            format!("tts: {}", provider(&facts.tts)),
        ]
        .join("\n")
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(device: Option<&str>) -> Facts {
        Facts {
            device: device.map(str::to_owned),
            stt: Ok("local".to_owned()),
            tts: Ok("edge".to_owned()),
        }
    }

    #[test]
    fn enabling_without_a_microphone_is_refused() {
        let mut mode = VoiceMode::default();
        let config = VoiceModeConfig::default();
        assert_eq!(
            mode.run(Command::On, Some(&facts(None)), &config),
            NO_DEVICE
        );
        assert!(!mode.on);
    }

    #[test]
    fn enabling_without_speech_to_text_is_refused() {
        let mut mode = VoiceMode::default();
        let mut facts = facts(Some("Built-in Microphone"));
        facts.stt = Err("no STT provider available".to_owned());
        let said = mode.run(Command::On, Some(&facts), &VoiceModeConfig::default());
        assert_eq!(said, NO_STT);
        assert!(!mode.on);
    }

    #[test]
    fn status_lists_the_providers() {
        let mut mode = VoiceMode::default();
        let config = VoiceModeConfig::default();
        let facts = facts(Some("Built-in Microphone"));
        let _ = mode.run(Command::On, Some(&facts), &config);
        let status = mode.run(Command::Status, Some(&facts), &config);
        for line in ["voice: on", "record key: ctrl+b", "stt: local", "tts: edge"] {
            assert!(status.lines().any(|l| l == line), "{line} in {status}");
        }
    }

    #[test]
    fn auto_tts_turns_spoken_replies_on_with_voice_mode() {
        let mut mode = VoiceMode::default();
        let config = VoiceModeConfig {
            auto_tts: true,
            ..VoiceModeConfig::default()
        };
        let _ = mode.run(Command::Toggle, Some(&facts(Some("mic"))), &config);
        assert!(mode.on && mode.spoken);
        assert_eq!(mode.run(Command::Toggle, None, &config), "Voice mode off.");
        assert!(!mode.on);
    }
}
