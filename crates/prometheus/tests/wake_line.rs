//! The wake word in the line interface (spec: voice-mode, Wake word
//! listener; Wake engines and false-trigger control; Single wake listener
//! ownership across surfaces; Wake and voice diagnostics), driven through
//! a pseudo-terminal against recorded audio (the debug-build
//! `PMT_TEST_AUDIO_DIR` seam, whose stand-in engine hears the wake phrase
//! in the first recording), a command transcriber and the mock model.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::pty::{exit, prompt, spawn_with};
use common::{ok, run, temp_home};
use expectrl::Expect;
use expectrl::session::OsSession;

/// A home on the mock model hearing `heard`, speaking through a command
/// provider (or `tts` when given), with `extra` config appended.
fn wake_home(heard: &str, tts: &str, extra: &str) -> common::TestHome {
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
    let text = std::fs::read_to_string(&config).unwrap()
        + &format!(
            "\n[stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
             command = \"printf '{heard}'\"\n\n[tts]\nprovider = \"{tts}\"\n\n\
             [tts.providers.fake]\ntype = \"command\"\ncommand = \"cp {{input_path}} {{output_path}}\"\n\n{extra}"
        );
    std::fs::write(&config, text).unwrap();
    home
}

/// The recorded-audio directory: the wake phrase, then the request.
fn audio_dir(home: &Path, recordings: &[&str]) -> PathBuf {
    let dir = home.join("audio");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("device"), "Desk Microphone").unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-voice/tests/fixtures");
    for (n, name) in recordings.iter().enumerate() {
        std::fs::copy(fixtures.join(name), dir.join(format!("mic-{n:02}.wav"))).unwrap();
    }
    dir
}

fn line(home: &Path, audio: &Path) -> OsSession {
    let audio = audio.to_string_lossy().into_owned();
    let mut p = spawn_with(
        home,
        &["--no-color", "--set", "ui.interface=line"],
        &[("PMT_TEST_AUDIO_DIR", &audio)],
    );
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p
}

fn config_get(home: &Path, key: &str) -> String {
    let out = run(home, &["config", "get", key]);
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn a_hands_free_request_opens_a_session_is_answered_aloud_and_listening_resumes() {
    let heard = "What is on my calendar";
    let home = wake_home(heard, "fake", "[wake]\nenabled = true\n");
    let audio = audio_dir(home.path(), &["question.wav", "pause.wav"]);
    let mut p = line(home.path(), &audio);
    p.expect("Wake word armed: say \"hey prometheus\"").unwrap();
    p.expect("Heard \"hey prometheus\"").unwrap();
    p.expect(format!("mock: {heard}").as_str()).unwrap();
    prompt(&mut p);
    p.send_line("/wake status").unwrap();
    for line in [
        "wake: armed",
        "phrase: hey prometheus",
        "owner: this CLI",
        "capture: local",
        "input device: Desk Microphone",
        "audio host API: recorded fixtures",
    ] {
        p.expect(line).unwrap();
    }
    prompt(&mut p);
    exit(p);
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    assert!(
        played.contains(&format!("mock: {heard}")),
        "answered aloud: {played}"
    );
    // The wake recording and the request were both used; the request ran
    // in a new session (two sessions in the store).
    assert!(audio.join("mic-00.wav.used").exists() && audio.join("mic-01.wav.used").exists());
    let sessions = run(home.path(), &["sessions", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_slice(&sessions.stdout).unwrap();
    assert_eq!(
        listed["sessions"].as_array().map_or(0, Vec::len),
        2,
        "{listed}"
    );
    assert!(
        !home.path().join("run/wake.lock").exists(),
        "released at exit"
    );
}

#[test]
fn wake_refuses_to_arm_without_tts_and_names_what_is_missing() {
    let home = wake_home("unused", "nowhere", "");
    let audio = audio_dir(home.path(), &[]);
    let mut p = line(home.path(), &audio);
    prompt(&mut p);
    p.send_line("/wake on").unwrap();
    p.expect("Wake word not armed: no text-to-speech (TTS) provider resolves")
        .unwrap();
    prompt(&mut p);
    p.send_line("/wake status").unwrap();
    p.expect("wake: off").unwrap();
    prompt(&mut p);
    exit(p);
    assert_eq!(config_get(home.path(), "wake.enabled"), "false");
}

#[test]
fn an_explicit_engine_is_not_swapped_for_another() {
    let home = wake_home("unused", "fake", "[wake]\nengine = \"porcupine\"\n");
    let audio = audio_dir(home.path(), &[]);
    let mut p = line(home.path(), &audio);
    prompt(&mut p);
    p.send_line("/wake on").unwrap();
    p.expect("wake engine porcupine cannot run: it needs the secret PORCUPINE_ACCESS_KEY")
        .unwrap();
    prompt(&mut p);
    p.send_line("/wake status").unwrap();
    p.expect("wake: off").unwrap();
    p.expect("engine: unavailable (wake engine porcupine cannot run")
        .unwrap();
    prompt(&mut p);
    exit(p);
}

/// Two line interfaces attached to the daemon (an in-process chat holds
/// the home, so a second surface on this machine is a client of it).
#[test]
fn a_second_terminal_does_not_steal_the_microphone_and_a_silent_one_is_reported() {
    let home = wake_home("unused", "fake", "");
    let audio = audio_dir(home.path(), &[]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let mut first = line(home.path(), &audio);
    first.expect("attached to the gateway daemon").unwrap();
    prompt(&mut first);
    first.send_line("/wake on").unwrap();
    first.expect("Wake word armed").unwrap();
    prompt(&mut first);
    assert_eq!(
        config_get(home.path(), "wake.enabled"),
        "true",
        "saved by /wake on"
    );
    let mut second = line(home.path(), &audio);
    second.expect("attached to the gateway daemon").unwrap();
    // Armed at startup from wake.enabled: refused, the first owns it.
    second
        .expect("the wake listener is owned by the CLI (process")
        .unwrap();
    prompt(&mut second);
    second.send_line("/wake status").unwrap();
    second.expect("wake: off").unwrap();
    second.expect("owner: the CLI (process").unwrap();
    prompt(&mut second);
    exit(second);
    // The fixture microphone has nothing left to play: digital silence.
    std::thread::sleep(Duration::from_secs(2));
    first.send_line("/wake status").unwrap();
    first.expect("microphone delivers only silence").unwrap();
    prompt(&mut first);
    first.send_line("/wake off").unwrap();
    first.expect("Wake word off.").unwrap();
    prompt(&mut first);
    exit(first);
    assert_eq!(config_get(home.path(), "wake.enabled"), "false");
}

/// Another profile's phrase on a single-profile terminal: it says how to
/// reach that profile instead of switching.
#[test]
fn another_profiles_phrase_points_to_that_profile() {
    let home = wake_home("unused", "fake", "[wake]\nenabled = true\n");
    let audio = audio_dir(home.path(), &["question.wav"]);
    std::fs::write(audio.join("wake"), "hey coder\ncoder\n").unwrap();
    let mut p = line(home.path(), &audio);
    p.expect("Wake word armed").unwrap();
    p.expect("\"hey coder\" wakes profile coder: run `prometheus -p coder`")
        .unwrap();
    prompt(&mut p);
    exit(p);
}

#[test]
fn doctor_reports_the_wake_phrase_engine_device_and_host_api() {
    let home = wake_home(
        "unused",
        "fake",
        "[wake]\nengine = \"porcupine\"\nenabled = true\n",
    );
    let audio = audio_dir(home.path(), &[]);
    let out = common::command(home.path())
        .args(["doctor", "voice"])
        .env("PMT_TEST_AUDIO_DIR", &audio)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    for line in [
        "wake phrase \"hey prometheus\" (enabled)",
        "engine: wake engine porcupine cannot run: it needs the secret PORCUPINE_ACCESS_KEY",
        "owner: none",
        "capture: auto",
        "input device: Desk Microphone",
        "audio host API: recorded fixtures",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
}

/// The TUI owns the wake listener: a line interface asking for it is
/// told so and opens no microphone.
#[test]
fn a_line_interface_does_not_steal_the_tuis_microphone() {
    use common::tui::{enter, finish, opened, see, tui};
    let home = wake_home("unused", "fake", "");
    let audio = audio_dir(home.path(), &[]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let audio_env = audio.to_string_lossy().into_owned();
    let mut screen = tui(home.path(), &[], &[("PMT_TEST_AUDIO_DIR", &audio_env)]);
    screen.set_expect_timeout(Some(Duration::from_mins(1)));
    opened(&mut screen);
    enter(&mut screen, "/wake on");
    see(&mut screen, "Wake word armed");
    let mut cli = line(home.path(), &audio);
    cli.expect("attached to the gateway daemon").unwrap();
    // wake.enabled was saved by the TUI: arming at startup is refused.
    cli.expect("the wake listener is owned by the TUI (process")
        .unwrap();
    prompt(&mut cli);
    cli.send_line("/wake on").unwrap();
    cli.expect("the wake listener is owned by the TUI (process")
        .unwrap();
    prompt(&mut cli);
    exit(cli);
    enter(&mut screen, "/wake off");
    see(&mut screen, "Wake word off.");
    screen.send("\x04").unwrap();
    let (output, code) = finish(screen);
    assert_eq!(code, Some(0), "{output}");
}

/// The TUI answers a wake too: a new session, the request transcribed
/// and answered aloud.
#[test]
fn the_tui_answers_a_hands_free_request() {
    use common::tui::{finish, opened, see, tui};
    let heard = "Turn on the kitchen lights";
    let home = wake_home(heard, "fake", "[wake]\nenabled = true\n");
    let audio = audio_dir(home.path(), &["question.wav", "pause.wav"]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let audio_env = audio.to_string_lossy().into_owned();
    let mut screen = tui(home.path(), &[], &[("PMT_TEST_AUDIO_DIR", &audio_env)]);
    screen.set_expect_timeout(Some(Duration::from_mins(1)));
    opened(&mut screen);
    see(&mut screen, "Heard \"hey prometheus\"");
    see(&mut screen, &format!("mock: {heard}"));
    for _ in 0..100 {
        let played = std::fs::read_to_string(audio.join("played.log")).unwrap_or_default();
        if played.contains(&format!("mock: {heard}")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap_or_default();
    assert!(
        played.contains(&format!("mock: {heard}")),
        "answered aloud: {played}"
    );
    screen.send("\x04").unwrap();
    let (output, code) = finish(screen);
    assert_eq!(code, Some(0), "{output}");
}
