//! Barge-in and live streaming transcription against recorded audio
//! fixtures (spec: voice-mode, Barge-in; Live streaming transcription).
//! The fixture microphone's `barge-*.wav` files are what the user says
//! while a reply generates or plays.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::VoiceModeConfig;
use pmt_core::cancel::CancelToken;
use pmt_voice::barge::{INTERRUPTED_NOTE, Watch};
use pmt_voice::converse::{self, Ended, Host, Interjection, Kit};
use pmt_voice::device::{BoxFuture, Clip, Microphone, Player, Sound, Synth, Transcriber};
use pmt_voice::fixture::FixtureMicrophone;
use pmt_voice::pcm::{self, RATE};
use pmt_voice::speaker::Mouth;
use pmt_voice::stream::{Partial, StreamingTranscriber, SttStream};

fn fixture(name: &str) -> Vec<i16> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    pcm::read_wav(&std::fs::read(path).unwrap()).unwrap().1
}

/// Gaussian-free room noise: a low hum with RMS about 25.
fn room(ms: usize) -> Vec<i16> {
    (0..ms * 16)
        .map(|n| if n % 2 == 0 { 25 } else { -25 })
        .collect()
}

/// A microphone hearing `recordings` (`mic-*`) and, over replies,
/// `barges` (`barge-*`).
fn microphone(recordings: &[Vec<i16>], barges: &[Vec<i16>]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    std::fs::write(path.join("device"), "Fixture Microphone").unwrap();
    for (n, samples) in recordings.iter().enumerate() {
        std::fs::write(
            path.join(format!("mic-{n:02}.wav")),
            pcm::wav(samples, RATE),
        )
        .unwrap();
    }
    for (n, samples) in barges.iter().enumerate() {
        std::fs::write(
            path.join(format!("barge-{n:02}.wav")),
            pcm::wav(samples, RATE),
        )
        .unwrap();
    }
    (dir, path)
}

/// A quiet room, then the user talking.
fn talking_over(speech: &str) -> Vec<i16> {
    [room(400), fixture(speech)].concat()
}

/// Speech-to-text answering from a script.
struct Ears(Mutex<VecDeque<String>>, Mutex<usize>);

impl Ears {
    fn saying(lines: &[&str]) -> Arc<Self> {
        Arc::new(Self(
            Mutex::new(lines.iter().map(|l| (*l).to_owned()).collect()),
            Mutex::new(0),
        ))
    }

    fn calls(&self) -> usize {
        *self.1.lock().unwrap()
    }
}

impl Transcriber for Ears {
    fn transcribe<'a>(&'a self, _samples: &'a [i16]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            *self.1.lock().unwrap() += 1;
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "nothing scripted".to_owned())
        })
    }
}

struct Voice;

impl Synth for Voice {
    fn synthesize<'a>(&'a self, _sentence: &'a str) -> BoxFuture<'a, Result<Sound, String>> {
        Box::pin(async {
            Ok(Sound::Pcm {
                rate: RATE,
                samples: Vec::new(),
            })
        })
    }
}

/// Speakers where every clip takes a minute to play: what was started,
/// finished and cut short.
#[derive(Default)]
struct SlowSpeakers {
    log: Arc<Mutex<Vec<String>>>,
}

struct Cut {
    label: String,
    log: Arc<Mutex<Vec<String>>>,
    done: bool,
}

impl Drop for Cut {
    fn drop(&mut self) {
        if !self.done {
            self.log.lock().unwrap().push(format!("cut:{}", self.label));
        }
    }
}

impl Player for SlowSpeakers {
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if clip.label.starts_with("beep:") {
                return Ok(());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("start:{}", clip.label));
            let mut playing = Cut {
                label: clip.label.clone(),
                log: Arc::clone(&self.log),
                done: false,
            };
            tokio::time::sleep(Duration::from_mins(1)).await;
            playing.done = true;
            self.log.lock().unwrap().push(format!("end:{}", clip.label));
            Ok(())
        })
    }
}

/// A surface answering `reply`; with `slow`, generation runs until it is
/// cut. Presses the record key after `stop_after` turns.
struct Surface {
    reply: String,
    slow: bool,
    turns: Vec<String>,
    cuts: usize,
    stop_after: usize,
    /// A line typed once a reply has started playing.
    typing: Option<(String, Arc<Mutex<Vec<String>>>)>,
    typed: Vec<String>,
}

impl Host for Surface {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>, cut: CancelToken) {
        self.turns.push(text);
        if let Some(mouth) = mouth {
            mouth.text(&self.reply);
        }
        if self.slow {
            cut.cancelled().await;
        }
        if cut.is_cancelled() {
            self.cuts += 1;
        }
    }

    async fn interjection(&mut self) -> Interjection {
        if self.turns.len() >= self.stop_after {
            return Interjection::RecordKey;
        }
        if !self.turns.is_empty()
            && let Some((line, log)) = self.typing.take()
        {
            while !log.lock().unwrap().iter().any(|l| l.starts_with("start:")) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            return Interjection::Line(line);
        }
        std::future::pending().await
    }

    async fn typed(&mut self, line: String) -> bool {
        self.typed.push(line);
        true
    }

    fn heard(&mut self, _transcript: &str) {}
}

fn kit(mic: &Path, ears: &Arc<Ears>, player: Arc<dyn Player>) -> Kit {
    Kit {
        mic: Arc::new(FixtureMicrophone::new(mic)),
        ears: Arc::clone(ears) as _,
        streaming: None,
        partial: Arc::new(|_| {}),
        player,
        voice: Some(Arc::new(Voice)),
        meter: Arc::new(|_| {}),
        config: VoiceModeConfig::default(),
    }
}

#[tokio::test]
async fn talking_over_playback_stops_it_and_submits_the_utterance_with_a_note() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[talking_over("pause.wav")]);
    let ears = Ears::saying(&[
        "What time is it in Lisbon right now?",
        "Turn on the kitchen lights, please.",
    ]);
    let speakers = Arc::new(SlowSpeakers::default());
    let kit = kit(&mic, &ears, Arc::clone(&speakers) as _);
    let mut surface = Surface {
        reply: "It is three in the afternoon in Lisbon. Would you like the weather too?".to_owned(),
        slow: false,
        turns: Vec::new(),
        cuts: 0,
        stop_after: 2,
        typing: None,
        typed: Vec::new(),
    };
    let ended = tokio::time::timeout(
        Duration::from_secs(30),
        converse::converse(&mut surface, &kit),
    )
    .await
    .expect("the playback was cut short, not played for a minute");
    assert_eq!(ended, Ended::RecordKey);
    assert_eq!(surface.turns.len(), 2);
    assert_eq!(surface.turns[0], "What time is it in Lisbon right now?");
    assert_eq!(
        surface.turns[1],
        format!("{INTERRUPTED_NOTE}\n\nTurn on the kitchen lights, please.")
    );
    assert_eq!(surface.cuts, 0, "the turn had already finished generating");
    let log = speakers.log.lock().unwrap().clone();
    let first = "It is three in the afternoon in Lisbon.";
    assert!(log.contains(&format!("start:{first}")), "{log:?}");
    assert!(log.contains(&format!("cut:{first}")), "{log:?}");
    assert!(!log.iter().any(|l| l.starts_with("end:")), "{log:?}");
}

#[tokio::test]
async fn a_stop_phrase_said_while_the_reply_generates_interrupts_the_turn_and_ends() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[talking_over("question.wav")]);
    let ears = Ears::saying(&["Summarize the open tickets.", "Stop."]);
    let speakers = Arc::new(SlowSpeakers::default());
    let kit = kit(&mic, &ears, Arc::clone(&speakers) as _);
    let mut surface = Surface {
        reply: String::new(),
        slow: true,
        turns: Vec::new(),
        cuts: 0,
        stop_after: usize::MAX,
        typing: None,
        typed: Vec::new(),
    };
    let ended = tokio::time::timeout(
        Duration::from_secs(30),
        converse::converse(&mut surface, &kit),
    )
    .await
    .expect("the turn was interrupted");
    assert_eq!(ended, Ended::StopPhrase);
    assert_eq!(surface.turns, ["Summarize the open tickets."]);
    assert_eq!(surface.cuts, 1, "the generating turn was interrupted");
    assert_eq!(ears.calls(), 2);
}

#[tokio::test]
async fn typing_over_the_playback_stops_it() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[]);
    let ears = Ears::saying(&["What time is it in Lisbon right now?"]);
    let speakers = Arc::new(SlowSpeakers::default());
    let kit = kit(&mic, &ears, Arc::clone(&speakers) as _);
    let mut surface = Surface {
        reply: "It is three in the afternoon in Lisbon.".to_owned(),
        slow: false,
        turns: Vec::new(),
        cuts: 0,
        stop_after: usize::MAX,
        typing: Some(("and in Porto?".to_owned(), Arc::clone(&speakers.log))),
        typed: Vec::new(),
    };
    let ended = tokio::time::timeout(
        Duration::from_secs(30),
        converse::converse(&mut surface, &kit),
    )
    .await
    .expect("typing cut the playback");
    assert_eq!(ended, Ended::Silence);
    assert_eq!(surface.typed, ["and in Porto?"]);
    let log = speakers.log.lock().unwrap().clone();
    assert_eq!(
        log,
        [
            "start:It is three in the afternoon in Lisbon.",
            "cut:It is three in the afternoon in Lisbon."
        ]
    );
}

#[tokio::test]
async fn barge_in_off_leaves_the_reply_alone() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[talking_over("pause.wav")]);
    let ears = Ears::saying(&["What time is it in Lisbon right now?"]);
    let mut kit = kit(&mic, &ears, Arc::new(SlowSpeakers::default()));
    kit.config.barge_in = false;
    kit.voice = None;
    let mut surface = Surface {
        reply: String::new(),
        slow: false,
        turns: Vec::new(),
        cuts: 0,
        stop_after: 1,
        typing: None,
        typed: Vec::new(),
    };
    assert_eq!(
        converse::converse(&mut surface, &kit).await,
        Ended::RecordKey
    );
    assert_eq!(
        ears.calls(),
        1,
        "nothing heard over the reply was transcribed"
    );
    assert!(
        mic.join("barge-00.wav").exists(),
        "the monitor never opened"
    );
}

#[tokio::test]
async fn the_monitor_fires_within_300_ms_of_the_user_starting_to_speak() {
    let speech = fixture("question.wav");
    let onset = speech
        .chunks(320)
        .position(|frame| pcm::rms(frame) > 3.0 * 30.0)
        .unwrap();
    let (_dir, mic) = microphone(&[], &[talking_over("question.wav")]);
    let device = FixtureMicrophone::new(&mic).monitor().await.unwrap();
    let mut watch = Watch::new(device, 3.0);
    watch.calibrate().await;
    assert!(
        watch.barge().floor().unwrap() < 40.0,
        "the floor is the room's"
    );
    watch.triggered().await;
    let (_, heard) = watch.into_parts();
    // The utterance so far starts where the user started speaking, and the
    // trigger came before 300 ms of it had passed (plus one read).
    let latency_ms = heard.len() / 16;
    assert!(latency_ms <= 300 + 100, "{latency_ms} ms");
    assert!(onset > 0);
}

/// A streaming transcriber whose session breaks after `breaks_after`
/// frames (never when `None`), showing partial text.
struct Realtime {
    breaks_after: Option<usize>,
    text: String,
    opened: Mutex<usize>,
}

struct Session {
    frames: usize,
    breaks_after: Option<usize>,
    text: String,
    partial: Partial,
}

impl StreamingTranscriber for Realtime {
    fn open(&self, partial: Partial) -> BoxFuture<'_, Result<Box<dyn SttStream>, String>> {
        *self.opened.lock().unwrap() += 1;
        let session = Session {
            frames: 0,
            breaks_after: self.breaks_after,
            text: self.text.clone(),
            partial,
        };
        Box::pin(async move { Ok(Box::new(session) as Box<dyn SttStream>) })
    }
}

impl SttStream for Session {
    fn push(&mut self, _samples: &[i16]) -> Result<(), String> {
        self.frames += 1;
        if self.breaks_after.is_some_and(|n| self.frames > n) {
            return Err("the realtime socket closed".to_owned());
        }
        let words: Vec<&str> = self.text.split(' ').collect();
        let heard = (self.frames / 25).min(words.len());
        (self.partial)(&words[..heard].join(" "));
        Ok(())
    }

    fn finish(self: Box<Self>) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async move { Ok(self.text) })
    }
}

fn streaming_kit(
    mic: &Path,
    ears: &Arc<Ears>,
    realtime: Arc<Realtime>,
) -> (Kit, Arc<Mutex<Vec<String>>>) {
    let partials = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&partials);
    let mut kit = kit(mic, ears, Arc::new(SlowSpeakers::default()));
    kit.voice = None;
    kit.streaming = Some(realtime as Arc<dyn StreamingTranscriber>);
    kit.partial = Arc::new(move |text: &str| {
        let mut seen = seen.lock().unwrap();
        if seen.last().is_none_or(|last| last != text) {
            seen.push(text.to_owned());
        }
    });
    (kit, partials)
}

#[tokio::test]
async fn a_streaming_session_shows_partial_text_and_supplies_the_transcript() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[]);
    let ears = Ears::saying(&["file path, never reached"]);
    let realtime = Arc::new(Realtime {
        breaks_after: None,
        text: "What time is it in Lisbon right now?".to_owned(),
        opened: Mutex::new(0),
    });
    let (kit, partials) = streaming_kit(&mic, &ears, Arc::clone(&realtime));
    let heard = converse::listen(&kit).await.unwrap();
    assert_eq!(
        heard,
        converse::Heard::Speech("What time is it in Lisbon right now?".to_owned())
    );
    assert_eq!(ears.calls(), 0, "the file path was not needed");
    let partials = partials.lock().unwrap();
    assert!(partials.contains(&"What time".to_owned()), "{partials:?}");
}

#[tokio::test]
async fn a_streaming_failure_falls_back_to_the_saved_recording() {
    let (_dir, mic) = microphone(&[fixture("question.wav")], &[]);
    let ears = Ears::saying(&["What time is it in Lisbon right now?"]);
    let realtime = Arc::new(Realtime {
        breaks_after: Some(40),
        text: "What time".to_owned(),
        opened: Mutex::new(0),
    });
    let (kit, _) = streaming_kit(&mic, &ears, Arc::clone(&realtime));
    let heard = converse::listen(&kit).await.unwrap();
    assert_eq!(
        heard,
        converse::Heard::Speech("What time is it in Lisbon right now?".to_owned())
    );
    assert_eq!(*realtime.opened.lock().unwrap(), 1);
    assert_eq!(
        ears.calls(),
        1,
        "the whole recording was transcribed instead"
    );
}
