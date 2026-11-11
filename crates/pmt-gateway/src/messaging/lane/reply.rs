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
use pmt_store::gateway::outbox::{NewOutbound, OutboundAttachment, OutboundKind, OutboundState};

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
    /// The input was addressed to the bot.
    pub(super) addressed: bool,
}

/// Final responses that mean "say nothing" (spec: messaging-gateway,
/// Intentional silence tokens), compared trimmed and case-normalized.
const SILENCE: &[&str] = &[
    "[silent]", "silent", "no_reply", "no reply", "[静默]", "静默", "[沉默]", "沉默",
];

/// Whether `text` is exactly an intentional silence token.
pub(crate) fn is_silence(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    SILENCE.contains(&normalized.as_str())
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
    fn answer_text(&self, finished: &TurnFinished) -> Option<String> {
        match (&finished.final_response, &finished.error) {
            (Some(answer), _) if !answer.trim().is_empty() => Some(answer.clone()),
            (_, Some(error)) => Some(pmt_i18n::t!(
                self.strings(),
                "gateway.turn_failed",
                why = error.message
            )),
            _ => None,
        }
    }

    /// Delivers the answer of a finished turn.
    pub(super) async fn deliver_answer(&self, answer: Answer<'_>) {
        let Some(text) = self.answer_text(answer.finished) else {
            return;
        };
        if answer.finished.error.is_none() && is_silence(&text) {
            self.stay_silent(&answer, &text);
            return;
        }
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
        let speech = self.shared.agents.speech(&self.profile);
        voice::voice_notes(speech.as_deref(), &mut files).await;
        let (text, pins) = crate::messaging::geo::extract(&text);
        let text = if descriptor.sends(MediaKind::Location) {
            files.extend(pins.iter().map(crate::messaging::geo::Location::attachment));
            text
        } else {
            // A map link where the platform has no location pins.
            [text]
                .into_iter()
                .chain(pins.iter().map(crate::messaging::geo::Location::link))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let footer = self.footer(answer.session, &descriptor).await;
        let text = [text]
            .into_iter()
            .chain(notes)
            .chain(footer)
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

    /// An intentionally silent answer: nothing is sent (the outbox records
    /// it `silent`), unless the input was addressed to the bot, which then
    /// hears "(nothing to add)".
    fn stay_silent(&self, answer: &Answer<'_>, text: &str) {
        let mut new = NewOutbound {
            idempotency_key: format!("{}:{}:0", answer.session, answer.finished.turn_id),
            session_id: Some(answer.session.to_owned()),
            platform: self.routing.platform.clone(),
            chat_id: self.routing.chat_id.clone(),
            thread_id: self.routing.thread_id.clone(),
            reply_to: answer.reply_to.cloned(),
            edit_of: answer.draft.message_id.clone(),
            kind: OutboundKind::Reply,
            body: text.to_owned(),
            attachments: Vec::new(),
            notify: None,
        };
        if answer.addressed {
            self.strings()
                .get("gateway.nothing_to_add")
                .clone_into(&mut new.body);
            self.shared.deliver(&self.profile, &new);
            return;
        }
        let recorded = self
            .shared
            .store(&self.profile)
            .map(|store| store.record_unsent(&new, OutboundState::Silent));
        if let Some(Err(err)) = recorded {
            tracing::warn!("silent reply not recorded: {err}");
        }
    }

    /// The runtime footer of a final reply, when the platform's display
    /// policy or the chat's `/footer` asks for it: model, context use and
    /// working directory, in italics where the platform has them. It is
    /// added to what is sent only, never to the transcript.
    async fn footer(&self, session: &str, descriptor: &Descriptor) -> Option<String> {
        let store = self.shared.store(&self.profile)?;
        let chat = store
            .chat_prefs(&self.routing.platform, &self.routing.chat_id)
            .unwrap_or_default();
        if !chat.footer.unwrap_or_else(|| self.display().runtime_footer) {
            return None;
        }
        let row = session.parse().ok().and_then(|id| store.session(id).ok());
        let mut parts = Vec::new();
        if let Some(model) = row.as_ref().and_then(|s| s.model.clone()) {
            parts.push(model);
        }
        let usage = self
            .shared
            .agents
            .context_usage(&self.profile, session)
            .await;
        if let Ok(usage) = usage
            && let Some(window) = usage.context_window.filter(|w| *w > 0)
        {
            let percent = usage.tokens * 100 / window;
            parts.push(pmt_i18n::t!(
                self.strings(),
                "gateway.footer_context",
                percent = percent
            ));
        }
        if let Some(dir) = row.and_then(|s| s.cwd).and_then(|cwd| {
            PathBuf::from(cwd)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        }) {
            parts.push(dir);
        }
        if parts.is_empty() {
            return None;
        }
        let line = parts.join(" \u{b7} ");
        Some(match descriptor.markup {
            crate::messaging::adapter::Markup::Plain => line,
            _ => format!("_{line}_"),
        })
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
                notify: None,
            };
            self.shared.deliver(&self.profile, &new);
        }
    }

    /// A spoken copy of the reply as one voice file, when the chat's reply
    /// mode asks for one: a voice bubble where the platform sends them,
    /// else an audio attachment.
    async fn speak(&self, answer: &Answer<'_>, text: &str, descriptor: &Descriptor) {
        let Some(scope) = self.config() else { return };
        let (mode, _) = self.chat_voice(&self.routing);
        if !voice::speaks(mode, answer.voice) {
            return;
        }
        let Some(speech) = self.shared.agents.speech(&self.profile) else {
            return;
        };
        let dir = media::cache_dir(scope.dir());
        let _ = std::fs::create_dir_all(&dir);
        match speech.synthesize_billed(text, &dir).await {
            Ok((path, billed)) => {
                if let Some(billed) = &billed {
                    speech.bill(answer.session, billed);
                }
                let mut file = OutboundAttachment::new(&path.to_string_lossy());
                // A voice bubble where the platform sends them, else the
                // audio as a file.
                file.as_voice = descriptor.sends(MediaKind::Voice);
                voice::voice_notes(Some(speech.as_ref()), std::slice::from_mut(&mut file)).await;
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
                    notify: None,
                };
                self.shared.deliver(&self.profile, &new);
            }
            Err(err) => tracing::warn!("voice reply not synthesized: {err}"),
        }
    }

    /// Whether this turn's reply streams as audio: the reply mode speaks
    /// to this user and the adapter plays streamed audio.
    pub(super) fn streams_audio(&self, voice_input: bool) -> bool {
        let speaks = voice::speaks(self.chat_voice(&self.routing).0, voice_input);
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
        let (path, billed) = speech
            .synthesize_billed(sentence, &dir)
            .await
            .map_err(|err| {
                tracing::warn!("streamed speech not synthesized: {err}");
            })?;
        if let (Some(billed), Some(session)) = (&billed, self.session.as_deref()) {
            speech.bill(session, billed);
        }
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
