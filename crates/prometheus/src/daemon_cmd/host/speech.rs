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
use pmt_gateway::messaging::voice::{Billed, Speech};
use pmt_media::{MediaSession, MediaSetup};

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
    setup.runner = Arc::new(crate::chat::session::media::ProfileRunner(scope.clone()));
    // Voice notes and spoken replies are billed to their conversation.
    setup.usage = pmt_store::Store::open(scope)
        .ok()
        .map(|store| crate::chat::session::media::profile_usage_sink(Arc::new(store)));
    let cwd = scope.dir().join("workspace");
    setup.plugins = crate::chat::session::media_plugins::PluginMedia::shared(
        &crate::plugins_host::for_session(scope, &cwd).current(),
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
        Box::pin(async move { self.transcribe_billed(path).await.map(|(text, _)| text) })
    }

    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        out_dir: &'a Path,
    ) -> AdapterFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            self.synthesize_billed(text, out_dir)
                .await
                .map(|(path, _)| path)
        })
    }

    fn transcribe_billed<'a>(
        &'a self,
        path: &'a Path,
    ) -> AdapterFuture<'a, Result<(String, Option<Billed>), String>> {
        Box::pin(async move {
            let heard = self.0.transcribe(path).await?;
            let billed = billed(
                "transcribe",
                &heard.provider,
                &heard.model,
                &heard.billing_source,
            );
            Ok((heard.text, billed))
        })
    }

    fn synthesize_billed<'a>(
        &'a self,
        text: &'a str,
        out_dir: &'a Path,
    ) -> AdapterFuture<'a, Result<(PathBuf, Option<Billed>), String>> {
        Box::pin(async move {
            let spoken = self
                .0
                .synthesize(text, &pmt_media::speech::tts::Speak::default(), None)
                .await?;
            let billed = billed(
                "text_to_speech",
                &spoken.provider,
                &spoken.model,
                &spoken.billing_source,
            );
            let name = spoken.path.file_name().ok_or("no file name")?;
            let target = out_dir.join(name);
            if tokio::fs::rename(&spoken.path, &target).await.is_err() {
                tokio::fs::copy(&spoken.path, &target)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = tokio::fs::remove_file(&spoken.path).await;
            }
            Ok((target, billed))
        })
    }

    fn bill(&self, session: &str, billed: &Billed) {
        let served = (
            billed.provider.as_str(),
            billed.model.as_str(),
            billed.billing_source.as_str(),
        );
        self.0.bill(session, billed.tool, served);
    }

    fn voice_note<'a>(&'a self, path: &'a Path) -> AdapterFuture<'a, Option<PathBuf>> {
        Box::pin(async move {
            pmt_media::speech::audio::voice_note(&self.0.speech().ffmpeg, path).await
        })
    }
}

/// The call to bill: a metered one (not local).
fn billed(tool: &'static str, provider: &str, model: &str, billing_source: &str) -> Option<Billed> {
    (!billing_source.is_empty() && billing_source != "local").then(|| Billed {
        tool,
        provider: provider.to_owned(),
        model: model.to_owned(),
        billing_source: billing_source.to_owned(),
    })
}
