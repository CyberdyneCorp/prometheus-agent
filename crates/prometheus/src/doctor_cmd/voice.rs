//! The `voice` group (spec: voice-mode, Wake and voice diagnostics): the
//! wake phrase, engine, owner, capture mode, input device and audio host
//! API, with how to install a missing engine or recorder.

use pmt_core::doctor::{Check, Status};
use pmt_voice::wake::engine;
use pmt_voice::wake::owner;
use pmt_voice::wake::worker;

use super::Inputs;
use crate::chat::wake::{HostSupport, input_device};

/// The group's checks.
pub(super) async fn checks(inputs: &Inputs<'_>) -> Vec<Check> {
    let scope = inputs.scope;
    let config = &scope.config().wake;
    let test_audio = inputs.ctx.launch.test_audio_dir().cloned();
    let check =
        |name: &'static str, status, message: String| Check::new("voice", name, status, message);
    let mut checks = vec![check(
        "wake",
        Status::Ok,
        format!(
            "wake phrase \"{}\" ({})",
            config.phrase,
            if config.enabled { "enabled" } else { "off" }
        ),
    )];
    let support = HostSupport::of(scope, test_audio.is_some());
    checks.push(match engine::select(config, &support) {
        Ok(kind) => check("engine", Status::Ok, format!("engine: {}", kind.as_str())),
        Err(why) => {
            let status = if config.enabled {
                Status::Warn
            } else {
                Status::Ok
            };
            let hint = match config.engine {
                pmt_config::schema::WakeEngine::Auto => {
                    worker::install_hint(pmt_config::schema::WakeEngine::Openwakeword)
                }
                named => worker::install_hint(named),
            };
            check("engine", status, format!("engine: {why}")).with_hint(hint)
        }
    });
    checks.push(check(
        "owner",
        Status::Ok,
        format!(
            "owner: {}",
            owner::current(scope.home()).map_or_else(|| "none".to_owned(), |o| o.describe())
        ),
    ));
    checks.push(check(
        "capture",
        Status::Ok,
        format!("capture: {}", config.capture.as_str()),
    ));
    let audio = crate::chat::voice::audio(scope, test_audio.as_deref()).await;
    checks.push(match input_device(audio.mic.as_ref(), config).await {
        Ok((_, name)) => check("input device", Status::Ok, format!("input device: {name}")),
        Err(why) => check("input device", Status::Warn, format!("input device: {why}"))
            .with_hint("install ffmpeg (macOS, Windows) or parecord/arecord (Linux), then check the OS microphone permission".to_owned()),
    });
    checks.push(check(
        "audio host API",
        Status::Ok,
        format!("audio host API: {}", audio.mic.host_api()),
    ));
    checks
}
