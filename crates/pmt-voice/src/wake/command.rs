//! `/wake on|off|status` and the facts it reports (spec: voice-mode, Wake
//! word listener; Wake capture location for remote clients; Wake and
//! voice diagnostics). Arming needs both a speech-to-text and a
//! text-to-speech provider (a wake is answered aloud); the report names
//! the phrase, engine, owner, capture mode, input device and audio host
//! API, what is missing with how to install it, and a microphone that
//! only delivers silence.

use pmt_config::schema::{WakeCapture, WakeConfig, WakeEngine as Kind};

use super::listener::{Health, SILENT_MIC, permission_hint};
use super::owner::Owner;

/// A `/wake` subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `/wake on`.
    On,
    /// `/wake off`.
    Off,
    /// `/wake status` (bare `/wake` too).
    Status,
}

impl Command {
    /// Parses the arguments of `/wake`.
    ///
    /// # Errors
    ///
    /// The usage line.
    pub fn parse(args: &str) -> Result<Self, String> {
        Ok(match args.trim() {
            "on" => Self::On,
            "off" => Self::Off,
            "" | "status" => Self::Status,
            _ => return Err("Usage: /wake [on | off | status]".to_owned()),
        })
    }
}

/// Why `/wake on` refuses for want of speech providers: both must
/// resolve, and each missing one is named.
#[must_use]
pub fn providers_missing(
    stt: &Result<String, String>,
    tts: &Result<String, String>,
) -> Option<String> {
    let mut missing = Vec::new();
    if let Err(why) = stt {
        missing.push(format!("no speech-to-text provider resolves ({why})"));
    }
    if let Err(why) = tts {
        missing.push(format!("no text-to-speech (TTS) provider resolves ({why})"));
    }
    (!missing.is_empty()).then(|| {
        format!(
            "Wake word not armed: {}; configure them with `prometheus tools` (Voice)",
            missing.join(" and ")
        )
    })
}

/// Where wake audio is captured: `client` when configured, or under
/// `auto` when a client arms with `client_capture`.
#[must_use]
pub fn capture(configured: WakeCapture, client_capture: bool) -> WakeCapture {
    match configured {
        WakeCapture::Auto if client_capture => WakeCapture::Client,
        WakeCapture::Auto => WakeCapture::Local,
        other => other,
    }
}

/// The input device `wake.input_device` names among `devices`: an index,
/// or a case-insensitive substring matching exactly one name.
///
/// # Errors
///
/// No such index or name, or a substring matching several (named).
pub fn pick_device(devices: &[String], wanted: &str) -> Result<usize, String> {
    let wanted = wanted.trim();
    if let Ok(index) = wanted.parse::<usize>() {
        return if index < devices.len() {
            Ok(index)
        } else {
            Err(format!(
                "wake.input_device {index}: there are {} input devices",
                devices.len()
            ))
        };
    }
    let needle = wanted.to_lowercase();
    let matching: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, name)| name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    match matching.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!(
            "wake.input_device '{wanted}' matches no input device"
        )),
        many => Err(format!(
            "wake.input_device '{wanted}' is ambiguous: it matches {}",
            many.iter()
                .map(|i| format!("'{}'", devices[*i]))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What `/wake status` and `prometheus doctor` report.
#[derive(Debug, Clone)]
pub struct Facts {
    /// This surface's listener is armed.
    pub armed: bool,
    /// The engine `wake.engine` selects, or why none can run.
    pub engine: Result<Kind, String>,
    /// Who holds the listener.
    pub owner: Option<Owner>,
    /// This process (to say "this CLI").
    pub me: u32,
    /// The capture location.
    pub capture: WakeCapture,
    /// The input device used, or why none.
    pub device: Result<String, String>,
    /// The audio backend recording goes through.
    pub host_api: String,
    /// Missing engine or audio dependencies, with how to install them.
    pub missing: Vec<String>,
    /// The listener's health.
    pub health: Health,
}

/// The report.
#[must_use]
pub fn status(config: &WakeConfig, facts: &Facts) -> String {
    let owner = match &facts.owner {
        Some(owner) if owner.pid == facts.me => {
            format!("this {}", super::owner::surface_name(&owner.surface))
        }
        Some(owner) => owner.describe(),
        None => "none".to_owned(),
    };
    let mut lines = vec![
        format!("wake: {}", if facts.armed { "armed" } else { "off" }),
        format!("phrase: {}", config.phrase),
        format!(
            "engine: {}",
            match &facts.engine {
                Ok(kind) => kind.as_str().to_owned(),
                Err(why) => format!("unavailable ({why})"),
            }
        ),
        format!("owner: {owner}"),
        format!("capture: {}", facts.capture.as_str()),
        format!(
            "input device: {}",
            match &facts.device {
                Ok(name) => name.clone(),
                Err(why) => why.clone(),
            }
        ),
        format!("audio host API: {}", facts.host_api),
    ];
    lines.extend(facts.missing.iter().map(|m| format!("missing: {m}")));
    if facts.health.silent {
        lines.push(format!("{SILENT_MIC}: {}", permission_hint()));
    }
    if let Some(error) = &facts.health.error {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arming_names_the_missing_tts_provider() {
        let refused = providers_missing(
            &Ok("local".into()),
            &Err("no TTS provider available".into()),
        )
        .unwrap();
        assert!(refused.contains("text-to-speech (TTS)"), "{refused}");
        assert!(
            !refused.contains("speech-to-text provider resolves"),
            "{refused}"
        );
        assert!(providers_missing(&Ok("local".into()), &Ok("edge".into())).is_none());
    }

    #[test]
    fn input_devices_by_index_or_unambiguous_name() {
        let devices = vec![
            "MacBook Pro Microphone".to_owned(),
            "USB Microphone".to_owned(),
            "USB Headset".to_owned(),
        ];
        assert_eq!(pick_device(&devices, "1"), Ok(1));
        assert_eq!(pick_device(&devices, "headset"), Ok(2));
        let ambiguous = pick_device(&devices, "usb").unwrap_err();
        assert!(
            ambiguous.contains("'USB Microphone', 'USB Headset'"),
            "{ambiguous}"
        );
        assert!(pick_device(&devices, "7").is_err());
        assert!(pick_device(&devices, "webcam").is_err());
    }

    #[test]
    fn auto_capture_follows_the_arming_client() {
        assert_eq!(capture(WakeCapture::Auto, true), WakeCapture::Client);
        assert_eq!(capture(WakeCapture::Auto, false), WakeCapture::Local);
        assert_eq!(capture(WakeCapture::Local, true), WakeCapture::Local);
    }

    #[test]
    fn a_silent_microphone_is_reported_with_the_permission_hint() {
        let facts = Facts {
            armed: true,
            engine: Ok(Kind::Openwakeword),
            owner: None,
            me: 1,
            capture: WakeCapture::Local,
            device: Ok("Built-in Microphone".into()),
            host_api: "AVFoundation (ffmpeg)".into(),
            missing: Vec::new(),
            health: Health {
                silent: true,
                error: None,
            },
        };
        let report = status(&WakeConfig::default(), &facts);
        assert!(
            report.contains("microphone delivers only silence"),
            "{report}"
        );
        assert!(report.contains(permission_hint()), "{report}");
        assert!(report.contains("phrase: hey prometheus"), "{report}");
    }
}
