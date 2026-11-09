//! Voice mode against recorded audio fixtures, a scripted speech-to-text
//! provider and a logging player (spec: voice-mode, Push-to-talk
//! recording and continuous loop; Two-stage silence detection; Spoken and
//! typed stop phrases; Transcription hallucination filter;
//! Sentence-streamed spoken replies).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::VoiceModeConfig;
use pmt_voice::converse::{self, Ended, Heard, Host, Interjection, Kit};
use pmt_voice::device::{BoxFuture, Clip, Player, Sound, Synth, Transcriber};
use pmt_voice::fixture::{FixtureMicrophone, FixturePlayer};
use pmt_voice::pcm::{self, RATE};
use pmt_voice::record;
use pmt_voice::speaker::Mouth;
use pmt_voice::vad::End;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A microphone directory holding `recordings` in order.
fn microphone(recordings: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device"), "Fixture Microphone").unwrap();
    for (n, name) in recordings.iter().enumerate() {
        std::fs::copy(fixture(name), dir.path().join(format!("mic-{n:02}.wav"))).unwrap();
    }
    dir
}

/// Speech-to-text answering from a script, counting calls.
#[derive(Default)]
struct Ears {
    script: Mutex<VecDeque<String>>,
    heard_samples: Mutex<Vec<usize>>,
}

impl Ears {
    fn saying(lines: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(lines.iter().map(|l| (*l).to_owned()).collect()),
            heard_samples: Mutex::default(),
        })
    }

    fn calls(&self) -> usize {
        self.heard_samples.lock().unwrap().len()
    }
}

impl Transcriber for Ears {
    fn transcribe<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.heard_samples.lock().unwrap().push(samples.len());
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "nothing scripted".to_owned())
        })
    }
}

/// Text-to-speech making a short tone per sentence.
#[derive(Default)]
struct Voice {
    said: Mutex<Vec<String>>,
}

impl Synth for Voice {
    fn synthesize<'a>(&'a self, sentence: &'a str) -> BoxFuture<'a, Result<Sound, String>> {
        Box::pin(async move {
            self.said.lock().unwrap().push(sentence.to_owned());
            Ok(Sound::Pcm {
                rate: RATE,
                samples: pcm::beep(440.0, 20, 1),
            })
        })
    }
}

/// A player remembering what it played.
#[derive(Default)]
struct Speakers {
    played: Mutex<Vec<String>>,
}

impl Speakers {
    fn played(&self) -> Vec<String> {
        self.played.lock().unwrap().clone()
    }
}

impl Player for Speakers {
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.played.lock().unwrap().push(clip.label);
            Ok(())
        })
    }
}

fn kit(mic: &Path, ears: &Arc<Ears>, player: &Arc<Speakers>, voice: Option<Arc<Voice>>) -> Kit {
    Kit {
        mic: Arc::new(FixtureMicrophone::new(mic)),
        ears: Arc::clone(ears) as _,
        player: Arc::clone(player) as _,
        voice: voice.map(|v| v as Arc<dyn Synth>),
        meter: Arc::new(|_| {}),
        config: VoiceModeConfig::default(),
    }
}

/// A surface: answers every message with `reply`, streamed in two
/// deltas; presses the record key once `stop_after` turns ran.
struct Surface {
    reply: String,
    turns: Vec<String>,
    typed: Vec<String>,
    stop_after: Option<usize>,
    interjections: VecDeque<Interjection>,
}

impl Surface {
    fn new(reply: &str) -> Self {
        Self {
            reply: reply.to_owned(),
            turns: Vec::new(),
            typed: Vec::new(),
            stop_after: None,
            interjections: VecDeque::new(),
        }
    }
}

impl Host for Surface {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>) {
        self.turns.push(text);
        if let Some(mouth) = mouth {
            let (first, rest) = self.reply.split_at(self.reply.len() / 2);
            mouth.text(first);
            mouth.text(rest);
        }
    }

    async fn interjection(&mut self) -> Interjection {
        if self.stop_after.is_some_and(|n| self.turns.len() >= n) {
            return Interjection::RecordKey;
        }
        match self.interjections.pop_front() {
            Some(interjection) => interjection,
            None => std::future::pending().await,
        }
    }

    async fn typed(&mut self, line: String) -> bool {
        self.typed.push(line);
        true
    }

    fn heard(&mut self, _transcript: &str) {}
}

#[tokio::test]
async fn a_pause_between_words_keeps_the_recording_going() {
    let mic = microphone(&["pause.wav"]);
    let speakers = Arc::new(Speakers::default());
    let recording = record::record(
        &FixtureMicrophone::new(mic.path()),
        speakers.as_ref(),
        &VoiceModeConfig::default(),
        &(Arc::new(|_| {}) as record::Meter),
    )
    .await
    .unwrap();
    assert_eq!(recording.end, End::Silence);
    let fixture = pcm::read_wav(&std::fs::read(fixture("pause.wav")).unwrap())
        .unwrap()
        .1;
    // Everything said after the one-second pause is in the recording,
    // which then ran three more seconds of quiet.
    assert!(
        recording.samples.len() >= fixture.len(),
        "{}",
        recording.samples.len()
    );
    let tail = (fixture.len() - 16_000)..(fixture.len() - 9_600);
    assert!(pcm::rms(&recording.samples[tail]) > 500.0);
    assert_eq!(
        speakers.played(),
        ["beep:start", "beep:end"],
        "start beep, then the double end beep"
    );
}

#[tokio::test]
async fn nobody_speaking_ends_the_recording_and_submits_nothing() {
    let mic = microphone(&["room.wav"]);
    let ears = Ears::saying(&["should not be asked"]);
    let speakers = Arc::new(Speakers::default());
    let kit = kit(mic.path(), &ears, &speakers, None);
    let recording = record::record(
        kit.mic.as_ref(),
        kit.player.as_ref(),
        &kit.config,
        &kit.meter,
    )
    .await
    .unwrap();
    assert_eq!(recording.end, End::NoSpeech);
    assert_eq!(
        recording.samples.len(),
        15 * RATE as usize,
        "stopped at 15 s"
    );
    let mic = microphone(&["room.wav"]);
    let kit = self::kit(mic.path(), &ears, &speakers, None);
    assert_eq!(converse::listen(&kit).await.unwrap(), Heard::Nothing);
    assert_eq!(ears.calls(), 0);
}

#[tokio::test]
async fn the_conversation_continues_without_a_key_press() {
    let mic = microphone(&["question.wav", "pause.wav"]);
    let ears = Ears::saying(&[
        "What time is it in Lisbon right now?",
        "Turn on the kitchen lights, please.",
    ]);
    let speakers = Arc::new(Speakers::default());
    let voice = Arc::new(Voice::default());
    let kit = kit(mic.path(), &ears, &speakers, Some(Arc::clone(&voice)));
    let mut surface = Surface::new("It is three in the afternoon in Lisbon. Anything else?");
    surface.stop_after = Some(2);
    let ended = converse::converse(&mut surface, &kit).await;
    assert_eq!(ended, Ended::RecordKey);
    assert_eq!(
        surface.turns,
        [
            "What time is it in Lisbon right now?",
            "Turn on the kitchen lights, please."
        ]
    );
    // Each reply was spoken, sentence by sentence, before the next
    // recording started.
    let reply = ["It is three in the afternoon in Lisbon.", "Anything else?"];
    let cycle: Vec<&str> = ["beep:start", "beep:end"]
        .into_iter()
        .chain(reply)
        .collect();
    // The third recording started on its own; the record key stopped it.
    assert_eq!(
        speakers.played(),
        [cycle.clone(), cycle, vec!["beep:start"]].concat()
    );
    assert_eq!(*voice.said.lock().unwrap(), [reply, reply].concat());
}

#[tokio::test]
async fn three_quiet_recordings_end_the_loop() {
    let mic = microphone(&["room.wav", "room.wav", "room.wav", "question.wav"]);
    let ears = Ears::saying(&["never reached"]);
    let speakers = Arc::new(Speakers::default());
    let kit = kit(mic.path(), &ears, &speakers, None);
    let mut surface = Surface::new("unused");
    let ended = converse::converse(&mut surface, &kit).await;
    assert_eq!(ended, Ended::Silence);
    assert!(ended.message().starts_with("Listening stopped"));
    assert!(surface.turns.is_empty());
    assert_eq!(ears.calls(), 0);
}

#[tokio::test]
async fn a_phantom_outro_counts_as_no_speech() {
    let mic = microphone(&["question.wav", "question.wav", "question.wav"]);
    let ears = Ears::saying(&["Thank you for watching.", "you", "Bye. Bye. Bye."]);
    let speakers = Arc::new(Speakers::default());
    let kit = kit(mic.path(), &ears, &speakers, None);
    let mut surface = Surface::new("unused");
    assert_eq!(converse::converse(&mut surface, &kit).await, Ended::Silence);
    assert!(surface.turns.is_empty(), "nothing was submitted");
    assert_eq!(ears.calls(), 3);
}

#[tokio::test]
async fn a_request_starting_with_a_stop_word_is_sent_and_a_stop_phrase_ends() {
    let mic = microphone(&["question.wav", "question.wav", "question.wav"]);
    let ears = Ears::saying(&["stop the docker container", "Never mind.", "unused"]);
    let speakers = Arc::new(Speakers::default());
    let kit = kit(mic.path(), &ears, &speakers, None);
    let mut surface = Surface::new("Stopped.");
    assert_eq!(
        converse::converse(&mut surface, &kit).await,
        Ended::StopPhrase
    );
    assert_eq!(surface.turns, ["stop the docker container"]);
    assert_eq!(ears.calls(), 2);
}

#[tokio::test]
async fn a_typed_stop_phrase_ends_an_active_conversation_and_other_lines_are_typed_input() {
    let mic = microphone(&[]);
    let ears = Ears::saying(&[]);
    let speakers = Arc::new(Speakers::default());
    let kit = kit(mic.path(), &ears, &speakers, None);
    let mut surface = Surface::new("unused");
    surface.interjections = VecDeque::from([
        Interjection::Line("what about tomorrow".to_owned()),
        Interjection::Line("Goodbye!".to_owned()),
    ]);
    assert_eq!(
        converse::converse(&mut surface, &kit).await,
        Ended::StopPhrase
    );
    assert_eq!(surface.typed, ["what about tomorrow"]);
}

#[tokio::test]
async fn the_first_sentence_plays_before_the_rest_is_generated() {
    let speakers = Arc::new(Speakers::default());
    let voice = Arc::new(Voice::default());
    let mouth = Mouth::start(Arc::clone(&voice) as _, Arc::clone(&speakers) as _);
    mouth.text("The weather in Lisbon is sunny today. ");
    mouth.text("It will be twenty");
    tokio::time::timeout(Duration::from_secs(5), async {
        while speakers.played().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first sentence played while the reply was still streaming");
    assert_eq!(speakers.played(), ["The weather in Lisbon is sunny today."]);
    mouth.text(" degrees by noon. Enjoy the walk along the river!");
    mouth.finish().await;
    assert_eq!(
        speakers.played(),
        [
            "The weather in Lisbon is sunny today.",
            "It will be twenty degrees by noon.",
            "Enjoy the walk along the river!"
        ]
    );
}

#[tokio::test]
async fn a_fenced_code_block_is_not_read_aloud() {
    let speakers = Arc::new(Speakers::default());
    let voice = Arc::new(Voice::default());
    let mouth = Mouth::start(Arc::clone(&voice) as _, Arc::clone(&speakers) as _);
    for delta in [
        "Here is the script you asked for:\n``",
        "`bash\nfor f in *.log; do\n  gzip \"$f\"\ndone\n``",
        "`\nRun it in the logs directory.",
    ] {
        mouth.text(delta);
    }
    mouth.finish().await;
    let said = voice.said.lock().unwrap().join(" ");
    assert_eq!(
        said,
        "Here is the script you asked for: Run it in the logs directory."
    );
    assert!(!said.contains("gzip"));
}

#[tokio::test]
async fn the_fixture_player_logs_each_clip() {
    let dir = tempfile::tempdir().unwrap();
    let player = FixturePlayer::new(dir.path());
    player
        .play(Clip {
            label: "beep:start".to_owned(),
            sound: Sound::Pcm {
                rate: RATE,
                samples: vec![0; 10],
            },
        })
        .await
        .unwrap();
    let log = std::fs::read_to_string(dir.path().join("played.log")).unwrap();
    assert_eq!(log, "beep:start\tpcm 16000 Hz 10 samples\n");
}
