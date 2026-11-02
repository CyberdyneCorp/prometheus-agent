//! A finished turn's answer on the platform (spec: platform-adapters,
//! Fence-aware message splitting; Oversized output handling; Outbound
//! attachments and path safety; Voice replies and streaming TTS; Platform
//! configuration shape): `MEDIA:` lines become attachments (only from the
//! allowed roots), the text is split at the platform's limit in its unit
//! (unless the adapter splits itself), more than `gateway.delivery.
//! max_parts` parts become the first part plus the full text attached,
//! `reply_to_mode` decides which parts quote the question, and a spoken
//! reply follows when the voice reply mode asks for one and no streamed
//! audio already played it.

use std::path::PathBuf;

use pmt_config::schema::ReplyToMode;
use pmt_rpc::contracts::TurnFinished;
use pmt_store::gateway::outbox::{NewOutbound, OutboundAttachment, OutboundKind};

use super::Lane;
use super::turns::Draft;
use crate::messaging::adapter::{AudioOp, MediaKind};
use crate::messaging::adapters::Op;
use crate::messaging::attachments::{self, Roots};
use crate::messaging::descriptor::Descriptor;
use crate::messaging::shape::{self, Owner};
use crate::messaging::{deliverable, media, voice};

/// What a turn's reply needs from the turn.
pub(super) struct Answer<'a> {
    pub(super) session: &'a str,
    pub(super) finished: &'a TurnFinished,
    pub(super) draft: &'a Draft,
    pub(super) reply_to: Option<&'a String>,
    /// The user spoke.
    pub(super) voice: bool,
    /// The reply's audio already streamed to completion.
    pub(super) spoken: bool,
}

/// A reply's audio streamed sentence by sentence while it is written.
#[derive(Debug, Default)]
pub(crate) struct Spoken {
    stream_id: String,
    buffer: String,
    began: bool,
    failed: bool,
}

impl Spoken {
    pub(super) fn new(stream_id: &str) -> Self {
        Self {
            stream_id: stream_id.to_owned(),
            ..Self::default()
        }
    }
}

/// A file goes as the closest kind the platform sends: a document where
/// documents cannot be uploaded goes as its own kind, a voice note where
/// voice notes cannot be sent as audio.
fn fit_kinds(files: &mut [OutboundAttachment], descriptor: &Descriptor) {
    for file in files {
        if file.as_document && !descriptor.sends(MediaKind::Document) {
            file.as_document = false;
        }
        if file.as_voice && !descriptor.sends(MediaKind::Voice) {
            file.as_voice = false;
        }
    }
}

impl Lane {
    /// The text of a finished turn, or nothing to say.
    fn answer_text(finished: &TurnFinished) -> Option<String> {
        match (&finished.final_response, &finished.error) {
            (Some(answer), _) if !answer.trim().is_empty() => Some(answer.clone()),
            (_, Some(error)) => Some(format!("The turn failed: {}", error.message)),
            _ => None,
        }
    }

    /// Delivers the answer of a finished turn.
    pub(super) async fn deliver_answer(&self, answer: Answer<'_>) {
        let Some(text) = Self::answer_text(answer.finished) else {
            return;
        };
        let descriptor = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .unwrap_or_else(|| Descriptor::new(&self.routing.platform, 4096));
        let roots = self.roots(answer.session);
        let (text, files) = attachments::directives(&text);
        let detect = self
            .config()
            .is_none_or(|s| s.config().gateway.delivery.detect_paths);
        let (text, detected) = if detect {
            deliverable::extract(&text, &roots, &descriptor.media_out)
        } else {
            (text, Vec::new())
        };
        let (mut files, notes) = roots.filter(files);
        files.extend(detected);
        fit_kinds(&mut files, &descriptor);
        let text = [text]
            .into_iter()
            .chain(notes)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let parts = shape::parts(
            &self.shared,
            &self.profile,
            &descriptor,
            &text,
            &Owner {
                session: Some(answer.session),
                key: &answer.finished.turn_id,
            },
        );
        self.enqueue_parts(&answer, parts, &files);
        if !answer.spoken {
            self.speak(&answer, &text, &descriptor).await;
        }
    }

    /// Where attachments may come from in this session.
    fn roots(&self, session: &str) -> Roots {
        let workspace = self
            .shared
            .store(&self.profile)
            .zip(session.parse().ok())
            .and_then(|(store, id)| store.session(id).ok())
            .and_then(|s| s.cwd)
            .map(PathBuf::from);
        let dir = self
            .config()
            .map(|s| s.dir().to_path_buf())
            .unwrap_or_default();
        Roots {
            container: self.shared.agents.container_mounts(&self.profile, session),
            ..Roots::of_profile(&dir, workspace.as_deref())
        }
    }

    fn enqueue_parts(
        &self,
        answer: &Answer<'_>,
        parts: Vec<(String, Vec<OutboundAttachment>)>,
        files: &[OutboundAttachment],
    ) {
        let mode = self
            .config()
            .map(|s| {
                s.config()
                    .gateway
                    .platform(&self.routing.platform)
                    .reply_to()
            })
            .unwrap_or_default();
        let count = parts.len();
        for (index, (body, mut attached)) in parts.into_iter().enumerate() {
            if index + 1 == count {
                attached.extend(files.iter().cloned());
            }
            let quotes = match mode {
                ReplyToMode::Off => false,
                ReplyToMode::First => index == 0,
                ReplyToMode::All => true,
            };
            let new = NewOutbound {
                idempotency_key: format!("{}:{}:{index}", answer.session, answer.finished.turn_id),
                session_id: Some(answer.session.to_owned()),
                platform: self.routing.platform.clone(),
                chat_id: self.routing.chat_id.clone(),
                thread_id: self.routing.thread_id.clone(),
                reply_to: answer.reply_to.cloned().filter(|_| quotes),
                edit_of: if index == 0 {
                    answer.draft.message_id.clone()
                } else {
                    None
                },
                kind: OutboundKind::Reply,
                body,
                attachments: attached,
            };
            self.shared.deliver(&self.profile, &new);
        }
    }

    /// A spoken copy of the reply as one voice file, when the reply mode
    /// asks for one and the platform sends voice.
    async fn speak(&self, answer: &Answer<'_>, text: &str, descriptor: &Descriptor) {
        let Some(scope) = self.config() else { return };
        let mode = scope.config().gateway.voice.reply_mode;
        if !voice::speaks(mode, answer.voice) || !descriptor.sends(MediaKind::Voice) {
            return;
        }
        let Some(speech) = self.shared.agents.speech(&self.profile) else {
            return;
        };
        let dir = media::cache_dir(scope.dir());
        let _ = std::fs::create_dir_all(&dir);
        match speech.synthesize(text, &dir).await {
            Ok(path) => {
                let mut file = OutboundAttachment::new(&path.to_string_lossy());
                file.as_voice = true;
                let new = NewOutbound {
                    idempotency_key: format!(
                        "{}:{}:voice",
                        answer.session, answer.finished.turn_id
                    ),
                    session_id: Some(answer.session.to_owned()),
                    platform: self.routing.platform.clone(),
                    chat_id: self.routing.chat_id.clone(),
                    thread_id: self.routing.thread_id.clone(),
                    reply_to: None,
                    edit_of: None,
                    kind: OutboundKind::Reply,
                    body: String::new(),
                    attachments: vec![file],
                };
                self.shared.deliver(&self.profile, &new);
            }
            Err(err) => tracing::warn!("voice reply not synthesized: {err}"),
        }
    }

    /// Whether this turn's reply streams as audio: the reply mode speaks
    /// to this user and the adapter plays streamed audio.
    pub(super) fn streams_audio(&self, voice_input: bool) -> bool {
        let speaks = self
            .config()
            .is_some_and(|s| voice::speaks(s.config().gateway.voice.reply_mode, voice_input));
        speaks
            && self.shared.agents.speech(&self.profile).is_some()
            && self
                .shared
                .adapters
                .ready(&self.profile, &self.routing.platform)
                .is_some_and(|slot| slot.can(Op::Audio))
    }

    /// Speaks the complete sentences of streamed text (all of it when
    /// `last`); `true` once the stream finished cleanly.
    pub(super) async fn speak_streamed(&self, spoken: &mut Spoken, text: &str, last: bool) -> bool {
        if spoken.failed {
            return false;
        }
        spoken.buffer.push_str(text);
        let (mut sentences, rest) = voice::sentences(&spoken.buffer);
        spoken.buffer = rest;
        if last && !spoken.buffer.trim().is_empty() {
            sentences.push(std::mem::take(&mut spoken.buffer));
        }
        for sentence in sentences {
            if self.play(spoken, &sentence).await.is_err() {
                spoken.failed = true;
                let abort = AudioOp::Abort {
                    stream_id: spoken.stream_id.clone(),
                };
                let _ = self.audio(&abort).await;
                return false;
            }
        }
        if !last || !spoken.began {
            return false;
        }
        let finish = AudioOp::Finish {
            stream_id: spoken.stream_id.clone(),
        };
        self.audio(&finish).await.is_ok()
    }

    async fn play(&self, spoken: &mut Spoken, sentence: &str) -> Result<(), ()> {
        let speech = self.shared.agents.speech(&self.profile).ok_or(())?;
        let dir = self.config().map(|s| media::cache_dir(s.dir())).ok_or(())?;
        let _ = std::fs::create_dir_all(&dir);
        let path = speech.synthesize(sentence, &dir).await.map_err(|err| {
            tracing::warn!("streamed speech not synthesized: {err}");
        })?;
        let bytes = std::fs::read(&path).map_err(drop)?;
        let _ = std::fs::remove_file(&path);
        if !spoken.began {
            let begin = AudioOp::Begin {
                chat_id: self.routing.chat_id.clone(),
                stream_id: spoken.stream_id.clone(),
            };
            self.audio(&begin).await?;
            spoken.began = true;
        }
        let write = AudioOp::Write {
            stream_id: spoken.stream_id.clone(),
            bytes,
        };
        self.audio(&write).await
    }

    async fn audio(&self, op: &AudioOp) -> Result<(), ()> {
        let slot = self
            .shared
            .adapters
            .ready(&self.profile, &self.routing.platform)
            .ok_or(())?;
        match slot.adapter.audio(op).await {
            Ok(()) => Ok(()),
            Err(crate::messaging::adapter::SendError::Unsupported) => {
                slot.refused(Op::Audio);
                Err(())
            }
            Err(err) => {
                tracing::warn!("streamed audio failed: {}", err.describe());
                Err(())
            }
        }
    }
}
