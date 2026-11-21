//! The continuous voice conversation (spec: voice-mode, Push-to-talk
//! recording and continuous loop; Spoken and typed stop phrases;
//! Transcription hallucination filter).
//!
//! The record key starts [`converse`]: record, transcribe, submit the
//! transcript as a user message, speak the reply when spoken replies are
//! on, and record again without a key press. The loop ends when the
//! record key is pressed during a recording, when a stop phrase is said
//! (or typed while the conversation is active), or after
//! [`QUIET_RECORDINGS`] consecutive recordings without speech; a
//! transcript the hallucination filter drops counts as no speech.
//!
//! With `voice.barge_in` the microphone stays open while the reply
//! generates and plays ([`crate::barge`]): speaking interrupts the turn
//! or cuts the playback, and the new utterance (with a note that the
//! reply was cut off) becomes the next message; a stop phrase said over
//! the reply ends the conversation. A key press or typed line during the
//! playback stops it too. With `voice.stt_streaming` the recording is
//! also transcribed live ([`crate::stream`]).

use std::future::Future;
use std::sync::Arc;

use pmt_config::schema::VoiceModeConfig;
use pmt_core::cancel::CancelToken;

use crate::barge::{INTERRUPTED_NOTE, Watch};
use crate::device::{Capture, Microphone, Player, Synth, Transcriber};
use crate::filter;
use crate::record::{self, Meter};
use crate::speaker::Mouth;
use crate::stream::{Live, Partial, StreamingTranscriber};
use crate::vad::Limits;

/// Consecutive recordings without speech that end the conversation.
pub const QUIET_RECORDINGS: u32 = 3;

/// What the conversation runs with.
#[derive(Clone)]
pub struct Kit {
    /// The input device.
    pub mic: Arc<dyn Microphone>,
    /// Speech-to-text of a whole recording.
    pub ears: Arc<dyn Transcriber>,
    /// Live transcription while the user speaks (`voice.stt_streaming`
    /// with a provider that streams).
    pub streaming: Option<Arc<dyn StreamingTranscriber>>,
    /// Shows the live transcript so far.
    pub partial: Partial,
    /// Plays the cues and spoken replies.
    pub player: Arc<dyn Player>,
    /// Text-to-speech while spoken replies are on.
    pub voice: Option<Arc<dyn Synth>>,
    /// Shows the input level while recording.
    pub meter: Meter,
    /// `[voice]`.
    pub config: VoiceModeConfig,
}

/// What the user did while a recording ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interjection {
    /// Pressed the record key.
    RecordKey,
    /// Typed a line.
    Line(String),
}

/// The surface the conversation runs on.
pub trait Host {
    /// Runs one voice turn of `text` (on the voice route; spec:
    /// voice-mode, Voice turn model and reasoning); its reply's text
    /// deltas go to `mouth` when spoken replies are on. When `cut` is
    /// cancelled (the user spoke over the reply) the surface interrupts
    /// the turn and returns once it has ended.
    fn turn(
        &mut self,
        text: String,
        mouth: Option<Arc<Mouth>>,
        cut: CancelToken,
    ) -> impl Future<Output = ()>;
    /// Waits for the user to press the record key or type a line.
    fn interjection(&mut self) -> impl Future<Output = Interjection>;
    /// Handles a typed line that is not a stop phrase as typed input;
    /// `false` when the conversation must end (voice mode was turned off).
    fn typed(&mut self, line: String) -> impl Future<Output = bool>;
    /// Shows a transcript about to be sent.
    fn heard(&mut self, transcript: &str);
}

/// Why a conversation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The record key was pressed during a recording.
    RecordKey,
    /// A stop phrase was said or typed.
    StopPhrase,
    /// [`QUIET_RECORDINGS`] recordings in a row had no speech.
    Silence,
    /// A typed command turned voice mode off.
    VoiceOff,
    /// Recording or transcription failed.
    Failed(String),
}

impl Ended {
    /// The line the surface prints.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::RecordKey | Self::StopPhrase | Self::VoiceOff => {
                "Voice conversation ended.".to_owned()
            }
            Self::Silence => {
                format!("Listening stopped: no speech in {QUIET_RECORDINGS} recordings in a row.")
            }
            Self::Failed(why) => format!("Voice conversation ended: {why}"),
        }
    }
}

/// What one recording amounted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// A request.
    Speech(String),
    /// A stop phrase.
    Stop,
    /// Nothing (no speech, or a dropped hallucination).
    Nothing,
}

/// Records and transcribes one utterance and says what it is.
///
/// # Errors
///
/// The microphone or the transcriber failed.
pub async fn listen(kit: &Kit) -> Result<Heard, String> {
    let mut live = Live::open(kit.streaming.as_ref(), Arc::clone(&kit.partial)).await;
    let recording = record::record_with(
        kit.mic.as_ref(),
        kit.player.as_ref(),
        &kit.config,
        &kit.meter,
        &mut |frame| live.push(frame),
    )
    .await?;
    if !recording.has_speech() {
        return Ok(Heard::Nothing);
    }
    let transcript = live.finish(kit.ears.as_ref(), &recording.samples).await?;
    Ok(judge(&transcript, &kit.config))
}

/// Records the rest of an utterance that started over a reply (`heard`
/// is its start) and says what it is.
///
/// # Errors
///
/// The transcriber failed.
pub async fn hear_rest(
    kit: &Kit,
    device: Box<dyn Capture>,
    heard: Vec<i16>,
) -> Result<Heard, String> {
    let mut live = Live::open(kit.streaming.as_ref(), Arc::clone(&kit.partial)).await;
    let recording = record::capture(
        device,
        heard,
        Limits::of(&kit.config),
        &kit.meter,
        &mut |frame| live.push(frame),
    )
    .await?;
    if !recording.has_speech() {
        return Ok(Heard::Nothing);
    }
    let transcript = live.finish(kit.ears.as_ref(), &recording.samples).await?;
    Ok(judge(&transcript, &kit.config))
}

/// What a transcript means under `[voice]`.
#[must_use]
pub fn judge(transcript: &str, config: &VoiceModeConfig) -> Heard {
    let text = transcript.trim();
    if text.is_empty() || (config.hallucination_filter && filter::is_hallucination(text)) {
        return Heard::Nothing;
    }
    if filter::is_stop_phrase(text, &config.stop_phrases) {
        return Heard::Stop;
    }
    Heard::Speech(text.to_owned())
}

/// How a reply ended.
enum Reply {
    /// Generated and played to its end.
    Done,
    /// The user spoke over it: what they said.
    BargedIn(Result<Heard, String>),
    /// The user pressed the record key (to talk) or typed during the
    /// playback.
    Interjected(Interjection),
}

/// What the conversation does next.
enum Next {
    Listen,
    Say(String),
    End(Ended),
}

/// Runs the continuous conversation until it ends.
pub async fn converse<H: Host>(host: &mut H, kit: &Kit) -> Ended {
    let mut quiet = 0;
    loop {
        // Biased: a recording always starts (and cues) before a waiting
        // key press or line stops it, so the order of events is fixed.
        let heard = tokio::select! {
            biased;
            heard = listen(kit) => heard,
            interjection = host.interjection() => match interjected(host, kit, interjection).await {
                Next::End(ended) => return ended,
                Next::Listen | Next::Say(_) => continue,
            },
        };
        let mut next = match heard {
            Ok(Heard::Speech(text)) => {
                quiet = 0;
                Next::Say(text)
            }
            Ok(Heard::Stop) => Next::End(Ended::StopPhrase),
            Ok(Heard::Nothing) => {
                quiet += 1;
                if quiet >= QUIET_RECORDINGS {
                    Next::End(Ended::Silence)
                } else {
                    Next::Listen
                }
            }
            Err(why) => Next::End(Ended::Failed(why)),
        };
        // A reply the user talks over leads straight to the next turn.
        while let Next::Say(text) = next {
            host.heard(&text);
            next = match respond(host, kit, text).await {
                // The record key over the playback only silences it.
                Reply::Done
                | Reply::BargedIn(Ok(Heard::Nothing))
                | Reply::Interjected(Interjection::RecordKey) => Next::Listen,
                Reply::BargedIn(Ok(Heard::Speech(text))) => {
                    Next::Say(format!("{INTERRUPTED_NOTE}\n\n{text}"))
                }
                Reply::BargedIn(Ok(Heard::Stop)) => Next::End(Ended::StopPhrase),
                Reply::BargedIn(Err(why)) => Next::End(Ended::Failed(why)),
                Reply::Interjected(line) => interjected(host, kit, line).await,
            };
        }
        if let Next::End(ended) = next {
            return ended;
        }
    }
}

/// The record key or a typed line between recordings.
async fn interjected<H: Host>(host: &mut H, kit: &Kit, interjection: Interjection) -> Next {
    match interjection {
        Interjection::RecordKey => Next::End(Ended::RecordKey),
        Interjection::Line(line) => {
            if filter::is_stop_phrase(&line, &kit.config.stop_phrases) {
                return Next::End(Ended::StopPhrase);
            }
            if host.typed(line).await {
                Next::Listen
            } else {
                Next::End(Ended::VoiceOff)
            }
        }
    }
}

/// Opens the barge-in watch of a turn and measures the quiet room, before
/// anything of the reply plays; `None` when barge-in is off or the
/// microphone cannot open.
async fn watch(kit: &Kit) -> Option<Watch> {
    if !kit.config.barge_in {
        return None;
    }
    let device = match kit.mic.monitor().await {
        Ok(device) => device,
        Err(why) => {
            tracing::debug!(%why, "no barge-in: the microphone did not open");
            return None;
        }
    };
    let mut watch = Watch::new(device, kit.config.barge_in_threshold_multiplier);
    watch.calibrate().await;
    Some(watch)
}

/// Runs the turn of `text` and plays its reply, listening for the user
/// talking over it.
async fn respond<H: Host>(host: &mut H, kit: &Kit, text: String) -> Reply {
    let mut watch = watch(kit).await;
    let mouth = kit
        .voice
        .as_ref()
        .map(|voice| Mouth::start(Arc::clone(voice), Arc::clone(&kit.player)));
    let cut = CancelToken::new();
    {
        let turn = host.turn(text, mouth.clone(), cut.clone());
        tokio::pin!(turn);
        let barged = if let Some(watch) = &mut watch {
            tokio::select! {
                biased;
                () = &mut turn => false,
                () = watch.triggered() => true,
            }
        } else {
            (&mut turn).await;
            false
        };
        if barged {
            if let Some(mouth) = &mouth {
                mouth.stop();
            }
            cut.cancel();
            turn.await;
            return barged_in(kit, watch).await;
        }
    }
    let Some(mouth) = mouth else {
        return Reply::Done;
    };
    let barged = tokio::select! {
        biased;
        () = mouth.finish() => return Reply::Done,
        () = triggered(watch.as_mut()) => true,
        interjection = host.interjection() => {
            mouth.stop();
            return Reply::Interjected(interjection);
        }
    };
    mouth.stop();
    debug_assert!(barged);
    barged_in(kit, watch).await
}

async fn triggered(watch: Option<&mut Watch>) {
    match watch {
        Some(watch) => watch.triggered().await,
        None => std::future::pending().await,
    }
}

/// The utterance that interrupted the reply.
async fn barged_in(kit: &Kit, watch: Option<Watch>) -> Reply {
    let Some(watch) = watch else {
        return Reply::Done;
    };
    let (device, heard) = watch.into_parts();
    Reply::BargedIn(hear_rest(kit, device, heard).await)
}
