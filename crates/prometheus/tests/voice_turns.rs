//! Voice turns in the line interface, in process and attached to a daemon
//! (spec: voice-mode, Voice turn model and reasoning; Recording retention
//! and privacy), against recorded audio (the debug-build
//! `PMT_TEST_AUDIO_DIR` seam), a command transcriber and the mock model.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::pty::{exit, prompt, spawn_with};
use common::{ok, run, temp_home};
use expectrl::Expect;
use expectrl::session::OsSession;

/// A home on `mock/alpha` with `[auxiliary.voice_chat] model = "voice"`,
/// a transcriber that hears `heard`, and a mock script answering with the
/// model that served each turn; `extra` is appended to the config.
fn voice_route_home(heard: &str, extra: &str) -> common::TestHome {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "alpha"],
    ));
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("routes.toml"),
        "[[rule]]\nmodel = \"alpha\"\nrepeat = true\ntext = \"answered by alpha\"\n\n\
         [[rule]]\nmodel = \"voice\"\nrepeat = true\ntext = \"answered by voice\"\n",
    )
    .unwrap();
    let config = profile.join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap()
        + &format!(
            "\n[providers.mock]\nscript = \"routes.toml\"\n\n\
             [auxiliary.voice_chat]\nmodel = \"voice\"\n\n\
             [stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
             command = \"printf '{heard}'\"\n\n{extra}"
        );
    std::fs::write(&config, text).unwrap();
    home
}

/// The microphone hearing `recordings` once each.
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

/// `/voice on`, one spoken request answered by the voice route, then a
/// typed follow-up answered by the session's route.
fn speak_then_type(p: &mut OsSession, heard: &str) {
    prompt(p);
    p.send_line("/voice on").unwrap();
    p.expect("Voice mode on").unwrap();
    prompt(p);
    p.send("\x02").unwrap();
    p.expect(format!("› {heard}").as_str()).unwrap();
    p.expect("answered by voice").unwrap();
    p.expect("Listening stopped").unwrap();
    prompt(p);
    p.send_line("and a typed follow-up").unwrap();
    p.expect("answered by alpha").unwrap();
    prompt(p);
}

fn recordings(home: &Path) -> usize {
    std::fs::read_dir(home.join("profiles/default/cache/voice")).map_or(0, |dir| {
        dir.filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".wav"))
        })
        .count()
    })
}

#[test]
fn a_spoken_turn_runs_on_the_voice_route_and_a_typed_one_on_the_sessions() {
    let heard = "What time is it in Lisbon";
    let home = voice_route_home(heard, "");
    let audio = audio_dir(home.path(), &["question.wav"]);
    let mut p = line(home.path(), &audio);
    speak_then_type(&mut p, heard);
    // The voice route is a one-turn switch: no "back to" notice, and the
    // stored route is still the session's.
    exit(p);
    assert_eq!(recordings(home.path()), 0, "the recording was deleted");
}

#[test]
fn an_attached_spoken_turn_runs_on_the_voice_route() {
    let heard = "Turn on the kitchen lights";
    let home = voice_route_home(heard, "");
    let audio = audio_dir(home.path(), &["pause.wav"]);
    let _daemon = common::daemon::start(home.path(), &[]);
    let audio_env = audio.to_string_lossy().into_owned();
    let mut p = spawn_with(home.path(), &[], &[("PMT_TEST_AUDIO_DIR", &audio_env)]);
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p.expect("attached to the gateway daemon").unwrap();
    speak_then_type(&mut p, heard);
    exit(p);
}

#[test]
fn kept_recordings_stay_after_transcription() {
    let heard = "What time is it in Lisbon";
    let home = voice_route_home(heard, "[voice]\nkeep_recordings = true\n");
    let audio = audio_dir(home.path(), &["question.wav"]);
    let mut p = line(home.path(), &audio);
    speak_then_type(&mut p, heard);
    exit(p);
    assert_eq!(recordings(home.path()), 1, "the recording was kept");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = home.path().join("profiles/default/cache/voice");
        let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner-only");
    }
}

/// A conversation longer than the voice model's window keeps voice turns
/// on the session's route, saying so once.
#[test]
fn a_voice_model_too_small_for_the_conversation_is_skipped_with_one_notice() {
    let heard = "What time is it in Lisbon";
    let home = voice_route_home(heard, "[models.\"mock/voice\"]\ncontext_window = 100\n");
    let audio = audio_dir(
        home.path(),
        &["question.wav", "room.wav", "room.wav", "question.wav"],
    );
    let mut p = line(home.path(), &audio);
    prompt(&mut p);
    p.send_line("a typed opener").unwrap();
    p.expect("answered by alpha").unwrap();
    prompt(&mut p);
    p.send_line("/voice on").unwrap();
    p.expect("Voice mode on").unwrap();
    prompt(&mut p);
    p.send("\x02").unwrap();
    p.expect("longer than voice's context window").unwrap();
    p.expect("answered by alpha").unwrap();
    // The next spoken turn (after two quiet recordings) says nothing more.
    p.expect(format!("› {heard}").as_str()).unwrap();
    let second = p.expect("answered by alpha").unwrap();
    assert!(
        !String::from_utf8_lossy(second.before()).contains("longer than"),
        "the notice is shown once"
    );
    p.expect("Listening stopped").unwrap();
    prompt(&mut p);
    exit(p);
}
