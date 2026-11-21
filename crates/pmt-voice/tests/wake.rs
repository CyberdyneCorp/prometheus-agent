//! The wake listener against recorded audio fixtures and a stand-in
//! engine worker (spec: voice-mode, Wake word listener; Wake engines and
//! false-trigger control; Recording retention and privacy).

use std::path::Path;
use std::sync::{Arc, Mutex};

use pmt_config::schema::{VoiceModeConfig, WakeEngine as Kind};
use pmt_core::cancel::CancelToken;
use pmt_voice::converse::{Host, Interjection, Kit};
use pmt_voice::device::{BoxFuture, Clip, Microphone, Player, Sound, Synth, Transcriber};
use pmt_voice::fixture::FixtureMicrophone;
use pmt_voice::pcm::{self, RATE};
use pmt_voice::speaker::Mouth;
use pmt_voice::wake::engine::{Detection, WakeEngine};
use pmt_voice::wake::fixture::FixtureEngine;
use pmt_voice::wake::listener::{self, Answered, Health, SharedHealth};
use pmt_voice::wake::worker::{Init, WorkerEngine};

fn fixture(name: &str) -> Vec<i16> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    pcm::read_wav(&std::fs::read(path).unwrap()).unwrap().1
}

struct Ears(Mutex<Vec<usize>>);

impl Transcriber for Ears {
    fn transcribe<'a>(&'a self, samples: &'a [i16]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(samples.len());
            Ok("What is on my calendar today?".to_owned())
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

#[derive(Default)]
struct Speakers(Mutex<Vec<String>>);

impl Player for Speakers {
    fn play(&self, clip: Clip) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(clip.label);
            Ok(())
        })
    }
}

#[derive(Default)]
struct Surface {
    turns: Vec<String>,
}

impl Host for Surface {
    async fn turn(&mut self, text: String, mouth: Option<Arc<Mouth>>, _cut: CancelToken) {
        self.turns.push(text);
        if let Some(mouth) = mouth {
            mouth.text("You have a dentist appointment at three.");
        }
    }

    async fn interjection(&mut self) -> Interjection {
        std::future::pending().await
    }

    async fn typed(&mut self, _line: String) -> bool {
        true
    }

    fn heard(&mut self, _transcript: &str) {}
}

#[tokio::test]
async fn a_wake_phrase_is_answered_aloud_then_listening_resumes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device"), "Desk Microphone").unwrap();
    // Room noise, the wake phrase; then the request as the next recording.
    let wake = [vec![0; 16_000], fixture("question.wav")].concat();
    std::fs::write(dir.path().join("mic-00.wav"), pcm::wav(&wake, RATE)).unwrap();
    std::fs::write(
        dir.path().join("mic-01.wav"),
        pcm::wav(&fixture("question.wav"), RATE),
    )
    .unwrap();
    let mic = FixtureMicrophone::new(dir.path());
    let detection = Detection {
        phrase: "hey prometheus".to_owned(),
        profile: None,
    };
    let mut engine = FixtureEngine::new(detection.clone());
    let health: SharedHealth = Arc::default();
    let mut capture = mic.open().await.unwrap();
    let heard = listener::wait(capture.as_mut(), &mut engine, &health)
        .await
        .unwrap();
    assert_eq!(heard, detection);
    // Paused: the wake capture is dropped before the request is recorded.
    drop(capture);
    let ears = Arc::new(Ears(Mutex::default()));
    let speakers = Arc::new(Speakers::default());
    let kit = Kit {
        mic: Arc::new(FixtureMicrophone::new(dir.path())),
        ears: Arc::clone(&ears) as _,
        streaming: None,
        partial: Arc::new(|_| {}),
        player: Arc::clone(&speakers) as _,
        voice: Some(Arc::new(Voice)),
        meter: Arc::new(|_| {}),
        config: VoiceModeConfig::default(),
    };
    let mut surface = Surface::default();
    let answered = listener::answer(&mut surface, &kit).await.unwrap();
    assert_eq!(
        answered,
        Answered::Replied("What is on my calendar today?".to_owned())
    );
    assert_eq!(surface.turns, ["What is on my calendar today?"]);
    assert_eq!(
        *speakers.0.lock().unwrap(),
        [
            "beep:start",
            "beep:end",
            "You have a dentist appointment at three."
        ]
    );
    // Only the request was transcribed: the wake audio went nowhere.
    assert_eq!(ears.0.lock().unwrap().len(), 1);
    assert_eq!(*health.lock().unwrap(), Health::default());
    // Listening resumes on a fresh capture with a reset engine.
    engine.reset().await;
    assert!(dir.path().join("mic-01.wav.used").exists());
}

#[tokio::test]
async fn a_microphone_of_zeros_is_reported_as_silent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device"), "Desk Microphone").unwrap();
    let mic = FixtureMicrophone::new(dir.path());
    let mut capture = mic.open().await.unwrap();
    let mut engine = FixtureEngine::new(Detection {
        phrase: "hey prometheus".to_owned(),
        profile: None,
    });
    let health: SharedHealth = Arc::default();
    let watched = Arc::clone(&health);
    let waited = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        listener::wait(capture.as_mut(), &mut engine, &health),
    );
    let checker = async {
        while !watched.lock().unwrap().silent {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        _ = waited => panic!("the silent microphone never fired a wake"),
        () = checker => {}
    }
    assert!(health.lock().unwrap().silent);
}

/// A worker scoring each frame from a script: one ambient spike, a quiet
/// frame, then the phrase held over three frames.
#[cfg(unix)]
#[tokio::test]
async fn an_ambient_spike_does_not_fire_but_a_held_phrase_does() {
    let script = r#"
import json, sys
init = json.loads(sys.stdin.readline())
assert init["engine"] == "openwakeword" and init["threshold"] == 0.6
print(json.dumps({"ready": True, "frame": 1280}), flush=True)
scores = iter([0.97, 0.05, 0.7, 0.8, 0.9, 0.2])
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reset"):
        print(json.dumps({"ok": True}), flush=True); continue
    print(json.dumps({"score": next(scores, 0.0)}), flush=True)
"#;
    let mut command = tokio::process::Command::new("python3");
    command.args(["-c", script]);
    let init = Init {
        engine: Kind::Openwakeword.as_str(),
        model: "hey_jarvis".to_owned(),
        phrases: Vec::new(),
        threshold: 0.6,
        model_dir: String::new(),
        access_key: None,
    };
    let mut engine = WorkerEngine::start(command, &init, "hey prometheus", 3)
        .await
        .unwrap();
    assert_eq!(engine.frame_samples(), 1_280);
    let frame = vec![0i16; 1_280];
    let mut fired = Vec::new();
    for n in 0..6 {
        if engine.feed(&frame).await.unwrap().is_some() {
            fired.push(n);
        }
    }
    assert_eq!(fired, [4], "only the third frame of the held phrase fires");
}

/// A worker that cannot load its engine fails to arm with its error.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_that_cannot_load_reports_why() {
    let script = r#"
import json, sys
sys.stdin.readline()
print(json.dumps({"error": "porcupine could not load jarvis: invalid AccessKey"}), flush=True)
"#;
    let mut command = tokio::process::Command::new("python3");
    command.args(["-c", script]);
    let init = Init {
        engine: Kind::Porcupine.as_str(),
        model: "jarvis".to_owned(),
        phrases: Vec::new(),
        threshold: 0.4,
        model_dir: String::new(),
        access_key: Some("bad".to_owned()),
    };
    let Err(why) = WorkerEngine::start(command, &init, "jarvis", 3).await else {
        panic!("the worker armed");
    };
    assert!(why.contains("invalid AccessKey"), "{why}");
}
