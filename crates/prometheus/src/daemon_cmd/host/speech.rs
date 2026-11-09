//! The profile's speech providers for messaging (spec: platform-adapters,
//! Voice input transcription; Voice replies and streaming TTS;
//! web-and-media-tools, `text_to_speech` tool and delivery): voice
//! messages are transcribed and replies spoken with the profile's
//! `[stt]` and `[tts]` providers, and voice-bubble audio is converted to
//! Opus with ffmpeg.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_gateway::messaging::adapter::AdapterFuture;
use pmt_gateway::messaging::voice::Speech;
use pmt_media::speech::command::CommandRunner;
use pmt_media::{MediaSession, MediaSetup};

/// Command speech providers run in a sandbox of the profile with
/// `process.spawn`.
struct ProfileRunner(ProfileScope);

impl CommandRunner for ProfileRunner {
    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let cwd = cwd.to_string_lossy();
        let mut command = crate::hooks_host::sandboxed(&self.0, Path::new(shell), &[], Some(&cwd))
            .map_err(|e| e.message().to_owned())?;
        command.args([flag, line]);
        Ok(tokio::process::Command::from(command))
    }
}

struct ProfileSpeech(MediaSession);

/// The speech providers of `scope`, when it can build an outbound client.
#[must_use]
pub(super) fn of_profile(scope: &ProfileScope) -> Option<Arc<dyn Speech>> {
    let credentials = crate::chat::session::media::credentials(scope);
    let mut setup: MediaSetup = pmt_media::explain::inspection_setup(
        credentials,
        scope.dir(),
        Some(crate::chat::session::media::launcher(scope)),
    )?;
    "gateway".clone_into(&mut setup.surface);
    setup.runner = Arc::new(ProfileRunner(scope.clone()));
    let cwd = scope.dir().join("workspace");
    setup.plugins = crate::chat::session::media_plugins::PluginMedia::shared(
        crate::plugins_host::for_session(scope, &cwd)
            .current()
            .providers
            .clone(),
        pmt_plugins::host::CallInfo {
            session_id: None,
            surface: "gateway".to_owned(),
            cwd: Some(cwd.display().to_string()),
        },
    );
    let session = MediaSession::new(setup).with_ffmpeg(crate::chat::session::media::ffmpeg(scope));
    Some(Arc::new(ProfileSpeech(session)))
}

impl Speech for ProfileSpeech {
    fn transcribe<'a>(&'a self, path: &'a Path) -> AdapterFuture<'a, Result<String, String>> {
        Box::pin(async move { self.0.transcribe(path).await.map(|t| t.text) })
    }

    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        out_dir: &'a Path,
    ) -> AdapterFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let spoken = self
                .0
                .synthesize(text, &pmt_media::speech::tts::Speak::default(), None)
                .await?;
            let name = spoken.path.file_name().ok_or("no file name")?;
            let target = out_dir.join(name);
            if tokio::fs::rename(&spoken.path, &target).await.is_err() {
                tokio::fs::copy(&spoken.path, &target)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = tokio::fs::remove_file(&spoken.path).await;
            }
            Ok(target)
        })
    }

    fn voice_note<'a>(&'a self, path: &'a Path) -> AdapterFuture<'a, Option<PathBuf>> {
        Box::pin(async move {
            pmt_media::speech::audio::voice_note(&self.0.speech().ffmpeg, path).await
        })
    }
}
