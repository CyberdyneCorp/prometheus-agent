//! Running a `[tts.providers.<name>]` command for one chunk: the text goes
//! to `{input_path}`, the audio is read back from `{output_path}`. The
//! provider's `warm_command` runs before its first use in the session and
//! its `release_command` when the session releases it.

use std::sync::Arc;
use std::time::Duration;

use super::command::{self, render};
use super::tts::{Voice, command_values};
use super::{SpeechState, tempdir};
use crate::MediaSetup;

/// The audio of one chunk from the command provider `voice`.
///
/// # Errors
///
/// The template is invalid, the command failed, timed out or wrote no
/// audio.
pub async fn speak(
    setup: &MediaSetup,
    state: &SpeechState,
    voice: &Voice,
    text: &str,
) -> Result<Vec<u8>, String> {
    let config = voice.command.as_ref().ok_or("not a command provider")?;
    let idle = Duration::from_secs(config.timeout_secs);
    let release = config
        .release_command
        .clone()
        .map(|line| (line, setup.workspace.clone(), Arc::clone(&setup.runner)));
    if state.first_use(&voice.provider, release)
        && let Some(warm) = &config.warm_command
    {
        command::run(setup.runner.as_ref(), warm, &setup.workspace, idle, true)
            .await
            .map_err(|why| format!("tts provider '{}' warm_command: {why}", voice.provider))?;
    }
    let dir = tempdir(setup)?;
    let input = dir.join("input.txt");
    let format = config.output_format.extension();
    let output = dir.join(format!("speech out.{format}"));
    tokio::fs::write(&input, text)
        .await
        .map_err(|e| e.to_string())?;
    let line = render(
        &config.command,
        &command_values(voice, &input, &output, format),
    )
    .map_err(|why| format!("tts.providers.{}.command: {why}", voice.provider))?;
    let ran = command::run(setup.runner.as_ref(), &line, &setup.workspace, idle, true)
        .await
        .map_err(|why| format!("tts provider '{}': {why}", voice.provider));
    let bytes = tokio::fs::read(&output).await.unwrap_or_default();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let ran = ran?;
    if bytes.is_empty() {
        return Err(format!(
            "tts provider '{}' wrote no audio; stderr: {}",
            voice.provider,
            super::audio::tail(&ran.stderr)
        ));
    }
    Ok(bytes)
}
