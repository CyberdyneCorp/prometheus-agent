//! The kit's checks of the descriptor's remaining optional capabilities:
//! threads, delete, inbound and outbound media (one file per declared
//! kind), streamed reply audio and receipts. Each declared capability is
//! exercised against the mock platform; an undeclared one gets a skip
//! line, so the report names every capability the descriptor has.

use futures_util::StreamExt as _;

use super::{Kit, MockFile, Report, eventually, mime_of_name};
use crate::messaging::adapter::{Attachment, AudioOp, OutboundAttachment, Outgoing};
use crate::messaging::descriptor::MediaKind;

/// The file name the kit uses for a kind (its extension types it).
fn file_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "kit-image.png",
        MediaKind::Audio => "kit-audio.mp3",
        MediaKind::Voice => "kit-voice.ogg",
        MediaKind::Video => "kit-video.mp4",
        MediaKind::Document => "kit-document.pdf",
    }
}

/// Content unique to a kind and direction, so a file is recognised by it.
fn content(kind: MediaKind, direction: &str) -> Vec<u8> {
    format!("conformance {direction} {kind:?} bytes").into_bytes()
}

impl Kit {
    pub(super) async fn capabilities(&self, report: &mut Report) {
        if self.descriptor.supports_threads {
            report.add("threads", self.threads().await);
        } else {
            report.skip("threads", "not declared");
        }
        if self.descriptor.supports_delete {
            report.add("delete", self.delete().await);
        } else {
            report.skip("delete", "not declared");
        }
        if self.descriptor.media_in.is_empty() {
            report.skip("media in", "no media_in kind declared");
        } else {
            report.add("media in", self.media_in().await);
        }
        if self.descriptor.media_out.is_empty() {
            report.skip("media out", "no media_out kind declared");
        } else {
            report.add("media out", self.media_out().await);
        }
        if self.descriptor.streaming_tts {
            report.add("streaming tts", self.streaming_tts().await);
        } else {
            report.skip("streaming tts", "not declared");
        }
        let receipts = self.descriptor.delivery_receipts || self.descriptor.read_receipts;
        report.skip(
            "receipts",
            if receipts {
                "receipt events join the adapter contract in M3"
            } else {
                "not declared"
            },
        );
    }

    /// What an outbound file of `kind` shows as on this platform: a voice
    /// note is an audio file where a bot cannot post voice notes.
    fn shown_as(&self, kind: MediaKind) -> MediaKind {
        if kind == MediaKind::Voice && !self.platform.voice_notes_out() {
            MediaKind::Audio
        } else {
            kind
        }
    }

    async fn threads(&self) -> Result<(), String> {
        let (chat, thread) = self
            .platform
            .thread()
            .ok_or("supports_threads is declared but the mock platform has no thread")?;
        let text = "conformance thread";
        self.adapter
            .send(&Outgoing::text(&chat, Some(&thread), text))
            .await
            .map_err(|e| e.describe())?;
        let sent = self.platform.sent();
        let landed = sent
            .iter()
            .find(|m| m.text == text)
            .ok_or("never arrived")?;
        if landed.chat_id == chat && landed.thread_id.as_deref() == Some(&thread) {
            Ok(())
        } else {
            Err(format!(
                "a send to thread {thread} of {chat} landed in thread {:?} of {}",
                landed.thread_id, landed.chat_id
            ))
        }
    }

    async fn delete(&self) -> Result<(), String> {
        let chat = self.dm();
        let sent = self
            .adapter
            .send(&Outgoing::text(&chat, None, "delete me"))
            .await
            .map_err(|e| e.describe())?;
        let id = sent.message_id.ok_or("the send returned no message id")?;
        self.adapter
            .delete(&chat, None, &id)
            .await
            .map_err(|e| e.describe())?;
        self.platform
            .deleted()
            .contains(&id)
            .then_some(())
            .ok_or_else(|| "the platform shows no deletion".to_owned())
    }

    async fn media_in(&self) -> Result<(), String> {
        for kind in self.descriptor.media_in.clone() {
            self.one_media_in(kind)
                .await
                .map_err(|why| format!("{kind:?}: {why}"))?;
        }
        Ok(())
    }

    async fn one_media_in(&self, kind: MediaKind) -> Result<(), String> {
        let (name, bytes) = (file_name(kind), content(kind, "in"));
        let caption = format!("conformance media {kind:?}");
        self.platform
            .inject_media(kind, name, &bytes, &caption)
            .ok_or("media_in is declared but the mock platform cannot deliver files")?;
        if !eventually(|| self.texts(&caption) > 0).await {
            return Err("the message carrying the file never arrived".to_owned());
        }
        let inbound = self
            .link
            .accepted()
            .into_iter()
            .find(|i| i.text == caption)
            .ok_or("lost")?;
        let [attachment] = inbound.attachments.as_slice() else {
            return Err(format!(
                "arrived with {} attachments, want 1",
                inbound.attachments.len()
            ));
        };
        if attachment.kind != kind {
            return Err(format!("arrived as {:?}, want {kind:?}", attachment.kind));
        }
        let got = self.download(attachment).await?;
        (got == bytes)
            .then_some(())
            .ok_or_else(|| "open_media streamed other bytes than the platform holds".to_owned())
    }

    async fn download(&self, attachment: &Attachment) -> Result<Vec<u8>, String> {
        let mut stream = self
            .adapter
            .open_media(attachment)
            .await
            .map_err(|e| format!("open_media: {}", e.describe()))?;
        let mut got = Vec::new();
        while let Some(chunk) = stream.chunks.next().await {
            got.extend(chunk.map_err(|e| format!("open_media: {e}"))?);
        }
        Ok(got)
    }

    async fn media_out(&self) -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| format!("no scratch directory: {e}"))?;
        for kind in self.descriptor.media_out.clone() {
            self.one_media_out(dir.path(), kind)
                .await
                .map_err(|why| format!("{kind:?}: {why}"))?;
        }
        Ok(())
    }

    async fn one_media_out(&self, dir: &std::path::Path, kind: MediaKind) -> Result<(), String> {
        let (name, bytes) = (file_name(kind), content(kind, "out"));
        let path = dir.join(name);
        std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {name}: {e}"))?;
        let mut file = OutboundAttachment::new(&path.to_string_lossy());
        file.as_voice = kind == MediaKind::Voice;
        let chat = self.dm();
        let mut message = Outgoing::text(&chat, None, "");
        message.attachments.push(file);
        self.adapter
            .send(&message)
            .await
            .map_err(|e| e.describe())?;
        let files = self.platform.files();
        let arrived: Option<&MockFile> = files.iter().find(|f| f.bytes == bytes);
        let Some(arrived) = arrived else {
            return Err(format!(
                "{name} ({}) never arrived on the platform",
                mime_of_name(name)
            ));
        };
        let want = self.shown_as(kind);
        if arrived.chat_id != chat {
            Err(format!("arrived in chat {}, want {chat}", arrived.chat_id))
        } else if arrived.kind != want {
            Err(format!("arrived as {:?}, want {want:?}", arrived.kind))
        } else {
            Ok(())
        }
    }

    async fn streaming_tts(&self) -> Result<(), String> {
        let stream_id = "conformance-tts".to_owned();
        let bytes = b"conformance streamed audio".to_vec();
        let ops = [
            AudioOp::Begin {
                chat_id: self.dm(),
                stream_id: stream_id.clone(),
            },
            AudioOp::Write {
                stream_id: stream_id.clone(),
                bytes: bytes.clone(),
            },
            AudioOp::Finish { stream_id },
        ];
        for op in &ops {
            self.adapter.audio(op).await.map_err(|e| e.describe())?;
        }
        let played = eventually(|| self.platform.streamed_audio().contains(&bytes)).await;
        played
            .then_some(())
            .ok_or_else(|| "the platform played no streamed audio".to_owned())
    }
}
