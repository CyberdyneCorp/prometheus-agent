//! Voice mode in the line interface (spec: voice-mode, Voice mode toggle
//! and commands; Push-to-talk recording and continuous loop; Spoken and
//! typed stop phrases; Sentence-streamed spoken replies), driven through a
//! pseudo-terminal against recorded audio (the debug-build
//! `PMT_TEST_AUDIO_DIR` seam), a command speech-to-text provider and the
//! mock model.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{ok, run, temp_home};
use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

/// A home on the mock model with a command transcriber that hears
/// `heard`, speaking with `tts` (`fake`: a command copying the text it
/// gets).
fn voice_home(heard: &str, tts: &str) -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    let config = home.path().join("profiles/default/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let text = text
        + &format!(
            "\n[stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
         command = \"printf '{heard}'\"\n\n[tts]\nprovider = \"{tts}\"\n\n\
         [tts.providers.fake]\ntype = \"command\"\ncommand = \"cp {{input_path}} {{output_path}}\"\n"
        );
    std::fs::write(&config, text).unwrap();
    home
}

/// The recorded-audio directory: a microphone named `device` (none when
/// `None`) and the recordings it will hear, in order.
fn audio_dir(home: &Path, device: Option<&str>, recordings: &[&str]) -> PathBuf {
    let dir = home.join("audio");
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(device) = device {
        std::fs::write(dir.join("device"), device).unwrap();
    }
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-voice/tests/fixtures");
    for (n, name) in recordings.iter().enumerate() {
        std::fs::copy(fixtures.join(name), dir.join(format!("mic-{n:02}.wav"))).unwrap();
    }
    dir
}

fn spawn(home: &Path, audio: &Path) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args(["--set", "ui.interface=line"])
        .current_dir(home)
        .env("TERM", "xterm")
        .env("PMT_TEST_AUDIO_DIR", audio);
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_mins(1)));
    session
}

fn prompt(p: &mut OsSession) {
    p.expect("› ").unwrap();
}

fn exit(mut p: OsSession) {
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn voice_mode_needs_a_microphone_and_its_status_names_the_providers() {
    let home = voice_home("unused", "edge");
    let audio = audio_dir(home.path(), None, &[]);
    let mut p = spawn(home.path(), &audio);
    prompt(&mut p);
    p.send_line("/voice on").unwrap();
    p.expect("No audio input device found").unwrap();
    prompt(&mut p);
    p.send_line("/voice status").unwrap();
    p.expect("voice: off").unwrap();
    std::fs::write(audio.join("device"), "Desk Microphone").unwrap();
    prompt(&mut p);
    p.send_line("/voice on").unwrap();
    p.expect("Voice mode on: press ctrl+b to talk").unwrap();
    prompt(&mut p);
    p.send_line("/voice status").unwrap();
    for line in [
        "voice: on",
        "record key: ctrl+b",
        "input device: Desk Microphone",
        "stt: fake",
        "tts: edge",
    ] {
        p.expect(line).unwrap();
    }
    prompt(&mut p);
    exit(p);
}

#[test]
fn a_spoken_question_is_answered_aloud_and_listening_resumes_until_silence() {
    let home = voice_home("What time is it in Lisbon", "fake");
    let audio = audio_dir(home.path(), Some("Desk Microphone"), &["question.wav"]);
    let mut p = spawn(home.path(), &audio);
    prompt(&mut p);
    p.send_line("/voice on").unwrap();
    p.expect("Voice mode on").unwrap();
    prompt(&mut p);
    p.send_line("/voice tts").unwrap();
    p.expect("spoken replies: on").unwrap();
    prompt(&mut p);
    // Ctrl+B.
    p.send("\x02").unwrap();
    p.expect("› What time is it in Lisbon").unwrap();
    p.expect("mock: What time is it in Lisbon").unwrap();
    p.expect("Listening stopped: no speech in 3 recordings in a row.")
        .unwrap();
    prompt(&mut p);
    // Outside a voice conversation "stop" is an ordinary message.
    p.send_line("stop").unwrap();
    p.expect("mock: stop").unwrap();
    prompt(&mut p);
    exit(p);
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    let labels: Vec<&str> = played
        .lines()
        .map(|l| l.split('\t').next().unwrap_or_default())
        .collect();
    // The reply was spoken, then three more recordings started on their
    // own and heard nobody.
    assert_eq!(
        labels,
        [
            "beep:start",
            "beep:end",
            "mock: What time is it in Lisbon",
            "beep:start",
            "beep:end",
            "beep:start",
            "beep:end",
            "beep:start",
            "beep:end",
        ],
        "{played}"
    );
    // The recording was deleted once transcribed.
    let voice = home.path().join("profiles/default/cache/voice");
    let left = std::fs::read_dir(&voice).map_or(0, |d| {
        d.filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".wav"))
        })
        .count()
    });
    assert_eq!(left, 0);
}

/// The scripted reply of the daemon tests: three sentences streamed a
/// word every 300 ms (about 6 s), so the first sentence is complete
/// long before the rest has been generated.
const SENTENCES: [&str; 3] = [
    "The kitchen lights are now on.",
    "The hallway lights stay off as you asked.",
    "Anything else for the house tonight?",
];

/// [`voice_home`] whose mock model answers [`SENTENCES`] slowly.
fn streaming_home(heard: &str) -> tempfile::TempDir {
    let home = voice_home(heard, "fake");
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("voice-mock.toml"),
        format!(
            "latency_ms = 300\n\n[[rule]]\nuser = \"{heard}\"\nrepeat = true\ntext = \"{}\"\n",
            SENTENCES.join(" ")
        ),
    )
    .unwrap();
    let config = profile.join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap()
        + "\n[providers.mock]\nscript = \"voice-mock.toml\"\n";
    std::fs::write(&config, text).unwrap();
    home
}

/// Waits until `played.log` names the first sentence and returns when.
/// Meanwhile what `p` writes is read (and kept for later expects), so a
/// full terminal never stalls it.
fn first_sentence_played(p: &mut OsSession, audio: &Path) -> Instant {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        p.check(NEVER).unwrap();
        let played = std::fs::read_to_string(audio.join("played.log")).unwrap_or_default();
        if played.contains(SENTENCES[0]) {
            return Instant::now();
        }
        assert!(Instant::now() < deadline, "nothing spoken: {played}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A needle nothing on a screen matches.
const NEVER: &str = "\u{0}never shown\u{0}";

/// The reply was spoken sentence by sentence, in order, the first one
/// at least 2 s before the whole answer reached the screen at
/// `answered` (the rest takes about 4 s more to generate).
fn spoken_while_generating(audio: &Path, first: Instant, answered: Instant) {
    let early = answered.duration_since(first);
    assert!(
        early >= Duration::from_secs(2),
        "the first sentence played only {early:?} before the answer arrived"
    );
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    let spoken: Vec<&str> = played
        .lines()
        .filter_map(|l| l.strip_suffix("\tscratch file"))
        .collect();
    assert_eq!(spoken, SENTENCES, "{played}");
}

/// Attached to a running daemon the line interface records, transcribes
/// and speaks on this machine while the daemon runs the turn, and speaks
/// the reply from the streamed answer text as it generates.
#[test]
fn an_attached_line_interface_speaks_the_first_sentence_while_the_reply_generates() {
    let home = streaming_home("Turn on the kitchen lights");
    let audio = audio_dir(home.path(), Some("Desk Microphone"), &["pause.wav"]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let audio_env = audio.to_string_lossy().into_owned();
    let mut p = common::pty::spawn_with(home.path(), &[], &[("PMT_TEST_AUDIO_DIR", &audio_env)]);
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p.expect("attached to the gateway daemon").unwrap();
    prompt(&mut p);
    p.send_line("/voice").unwrap();
    p.expect("Voice mode on").unwrap();
    prompt(&mut p);
    p.send_line("/voice tts").unwrap();
    p.expect("spoken replies: on").unwrap();
    prompt(&mut p);
    p.send("\x02").unwrap();
    p.expect("› Turn on the kitchen lights").unwrap();
    let first = first_sentence_played(&mut p, &audio);
    // The attached interface prints the answer when the turn finishes.
    p.expect(SENTENCES[2]).unwrap();
    let answered = Instant::now();
    p.expect("Listening stopped").unwrap();
    prompt(&mut p);
    p.send_line("/voice off").unwrap();
    p.expect("Voice mode off.").unwrap();
    prompt(&mut p);
    exit(p);
    spoken_while_generating(&audio, first, answered);
}

/// The TUI holds the same conversation: in voice mode the record key
/// (which otherwise focuses the banner) records, and the reply is spoken
/// as it generates.
#[test]
fn the_tui_speaks_the_first_sentence_while_the_reply_generates() {
    use common::tui::{enter, finish, opened, see, tui};
    let home = streaming_home("What is on my calendar");
    let audio = audio_dir(home.path(), Some("Desk Microphone"), &["question.wav"]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let audio_env = audio.to_string_lossy().into_owned();
    let mut p = tui(home.path(), &[], &[("PMT_TEST_AUDIO_DIR", &audio_env)]);
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    opened(&mut p);
    enter(&mut p, "/voice on");
    see(&mut p, "Voice mode on");
    enter(&mut p, "/voice tts");
    see(&mut p, "spoken replies: on");
    p.send("\x02").unwrap();
    let first = first_sentence_played(&mut p, &audio);
    // The TUI shows the answer once its message is complete.
    see(&mut p, SENTENCES[2]);
    let answered = Instant::now();
    see(&mut p, "Listening stopped");
    p.send("\x04").unwrap();
    let (screen, code) = finish(p);
    assert_eq!(code, Some(0), "{screen}");
    spoken_while_generating(&audio, first, answered);
}
