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
use std::time::Duration;

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

/// Attached to a running daemon the line interface records, transcribes
/// and speaks on this machine while the daemon runs the turn.
#[test]
fn an_attached_line_interface_holds_the_same_conversation() {
    let home = voice_home("Turn on the kitchen lights", "fake");
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
    p.expect("mock: Turn on the kitchen lights").unwrap();
    p.expect("Listening stopped").unwrap();
    prompt(&mut p);
    p.send_line("/voice off").unwrap();
    p.expect("Voice mode off.").unwrap();
    prompt(&mut p);
    exit(p);
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    assert!(
        played.contains("mock: Turn on the kitchen lights\tscratch file"),
        "{played}"
    );
}

/// The TUI holds the same conversation: in voice mode the record key
/// (which otherwise focuses the banner) records, and the answer is
/// spoken.
#[test]
fn the_tui_holds_the_same_conversation() {
    use common::tui::{enter, finish, opened, see, tui};
    let home = voice_home("What is on my calendar", "fake");
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
    see(&mut p, "mock: What is on my calendar");
    see(&mut p, "Listening stopped");
    p.send("\x04").unwrap();
    let (screen, code) = finish(p);
    assert_eq!(code, Some(0), "{screen}");
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    assert!(
        played.contains("mock: What is on my calendar\tscratch file"),
        "{played}"
    );
}
