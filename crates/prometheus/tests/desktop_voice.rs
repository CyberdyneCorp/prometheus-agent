//! The desktop's voice controls end to end (spec: voice-mode, Desktop voice
//! controls; Client-direct voice for remote daemons; Realtime full-duplex
//! voice chat mode (upgrade); Wake word profile routing): the desktop page
//! in Chromium (`common/desktop.rs`) with a fake microphone that plays a
//! recorded utterance in a loop (one second of speech-level tone, three of
//! silence), the cloud speech providers answered by the spec at their real
//! URLs (the page calls them directly), and the realtime provider and wake
//! engine as local stand-ins on the daemon's side.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::path::{Path, PathBuf};

use common::dashboard::playwright;
use common::desktop::Setup;
use common::{mock_home, ok, run, run_with_stdin};

/// A key the cloud speech providers get from the page (direct path).
const SPEECH_KEY: &str = "sk-speech-direct";
/// The realtime key, which only the daemon may hold.
const REALTIME_KEY: &str = "sk-realtime-daemon-only";

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// The fake microphone's recording: 16 kHz mono, one second of a 440 Hz
/// tone at speech level, then three seconds of silence.
fn microphone(home: &Path) -> PathBuf {
    let rate = 16_000u32;
    let mut samples: Vec<i16> = (0..rate)
        .map(|i| {
            let t = f64::from(i) / f64::from(rate);
            #[allow(clippy::cast_possible_truncation)]
            let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16;
            s
        })
        .collect();
    samples.extend(std::iter::repeat_n(0, (rate * 3) as usize));
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    let data = u32::try_from(samples.len() * 2).unwrap();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    let path = home.join("microphone.wav");
    std::fs::write(&path, wav).unwrap();
    path
}

fn set_secret(home: &Path, profile: &str, name: &str, value: &str) {
    ok(&run_with_stdin(
        home,
        &["-p", profile, "secrets", "set", name],
        &format!("{value}\n"),
    ));
}

/// Creates `profile`, answered by the mock model like the default one.
fn mock_profile(home: &Path, profile: &str) {
    ok(&run(home, &["profile", "create", profile]));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home, &["-p", profile, "config", "set", key, value]));
    }
}

/// Appends `text` to `profile`'s `config.toml`.
fn append(home: &Path, profile: &str, text: &str) {
    let path = home.join(format!("profiles/{profile}/config.toml"));
    let config = std::fs::read_to_string(&path).unwrap() + text;
    std::fs::write(&path, config).unwrap();
}

/// Listening tuned for the fake microphone's one-second utterances.
const LISTEN: &str = "\n[voice]\nsilence_duration_ms = 600\nspeech_confirm_ms = 200\n\
                      no_speech_timeout_secs = 20\n";

/// OpenAI speech-to-text and text-to-speech with a key: the direct path.
fn cloud_speech(home: &Path, profile: &str, voice: &str) {
    append(
        home,
        profile,
        &format!("{voice}\n[stt]\nprovider = \"openai\"\n\n[tts]\nprovider = \"openai\"\n"),
    );
    set_secret(home, profile, "OPENAI_API_KEY", SPEECH_KEY);
}

fn drive(home: &Path, spec: &str, setup: Setup, extra: &[(&str, &str)]) {
    let _stop = Stop(home);
    let mic = microphone(home);
    let harness = common::desktop::start_with(home, setup);
    let mut env = vec![
        ("PMT_DESKTOP_HARNESS", harness.url.as_str()),
        ("PMT_FAKE_MIC", mic.to_str().unwrap()),
        ("PMT_SPEECH_KEY", SPEECH_KEY),
    ];
    env.extend_from_slice(extra);
    playwright(home, spec, &env);
}

/// Scenario: Read a message aloud. Scenario: Audio never crosses the
/// daemon link (a cloud STT and TTS provider with a key: the page calls
/// them itself and the daemon receives only transcript text). Dictation
/// fills the composer; a conversation is the chained loop with barge-in
/// (talking over the reply stops it and the next message carries the
/// interruption note).
#[test]
fn desktop_voice_controls_speak_to_cloud_providers_directly() {
    let home = mock_home();
    cloud_speech(home.path(), "default", LISTEN);
    drive(home.path(), "desktop-voice.spec.ts", Setup::default(), &[]);
    // The daemon never kept a recording: nothing was relayed to it.
    let voice = home.path().join("profiles/default/cache/voice");
    assert_eq!(std::fs::read_dir(&voice).map_or(0, Iterator::count), 0);
}

/// Scenario: Host-only provider relays (a command STT provider runs on
/// the daemon host: the recording goes to `voice.transcribe`). Controls
/// whose provider does not resolve are hidden.
#[test]
fn desktop_voice_relays_host_only_providers_and_hides_what_cannot_work() {
    let home = mock_home();
    append(
        home.path(),
        "default",
        &format!(
            "{LISTEN}\n[stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
             command = \"head -c 4 {{input_path}} | grep -q RIFF && printf 'relayed words'\"\n\n\
             [tts]\nprovider = \"openai\"\n"
        ),
    );
    mock_profile(home.path(), "mute");
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    // `mute` has neither: no STT resolves and OpenAI speech has no key.
    append(
        home.path(),
        "mute",
        "\n[stt]\nprovider = \"openai\"\n\n[tts]\nprovider = \"openai\"\n",
    );
    drive(
        home.path(),
        "desktop-voice-relay.spec.ts",
        Setup::default(),
        &[],
    );
}

/// Scenario: Realtime voice delegates work (the realtime model's one tool
/// runs an ordinary turn that reads the file with the agent's tools, and
/// its answer goes back for the voice to speak). Scenario: Key stays on
/// the daemon (the provider sees the key, the page never does). The voice
/// layer's tokens are their own usage rows.
#[test]
fn realtime_voice_delegates_to_the_agent_and_keeps_the_key_on_the_daemon() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let provider = runtime.block_on(realtime_provider());
    let home = mock_home();
    append(
        home.path(),
        "default",
        &format!(
            "{LISTEN}chat_mode = \"realtime\"\n\n[voice.realtime]\nbase_url = \"{}/v1\"\n\n\
             [stt]\nprovider = \"openai\"\n\n[tts]\nprovider = \"openai\"\n\n\
             [providers.mock]\nscript = \"voice-script.toml\"\n",
            provider.uri()
        ),
    );
    set_secret(home.path(), "default", "OPENAI_API_KEY", REALTIME_KEY);
    std::fs::write(
        home.path().join("profiles/default/voice-script.toml"),
        "[[rule]]\nuser = \"summarize notes.txt\"\n[[rule.tool_calls]]\nname = \"read_file\"\n\
         arguments = { path = \"notes.txt\" }\n\n[[rule]]\ntool_result = \"launch on friday\"\n\
         text = \"The notes say the launch is on Friday.\"\n\n[[rule]]\nrepeat = true\ntext = \"ok\"\n",
    )
    .unwrap();
    let workspace = home.path().join("profiles/default/workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "launch on friday\n").unwrap();
    drive(
        home.path(),
        "desktop-voice-realtime.spec.ts",
        Setup::default(),
        &[("PMT_REALTIME_KEY", REALTIME_KEY)],
    );
    // The provider created the call with the daemon's key.
    let requests = runtime.block_on(provider.received_requests()).unwrap();
    let created = requests
        .iter()
        .find(|r| r.url.path() == "/v1/realtime/calls")
        .expect("the daemon created the call");
    assert_eq!(
        created.headers.get("authorization").unwrap(),
        &format!("Bearer {REALTIME_KEY}")
    );
    // The voice layer's response is a row of its own.
    let db = rusqlite::Connection::open_with_flags(
        home.path().join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let rows: i64 = db
        .query_row(
            "SELECT count(*) FROM usage_ledger WHERE purpose = 'voice.realtime' \
             AND input_tokens = 40 AND output_tokens = 25",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}

async fn realtime_provider() -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/realtime/calls"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("location", "/v1/realtime/calls/rtc_desktop1")
                .insert_header("content-type", "application/sdp")
                .set_body_string("v=0\r\no=- realtime-answer\r\n"),
        )
        .mount(&server)
        .await;
    server
}

/// Scenario: Wake a different profile: the desktop's ear streams its
/// microphone to the daemon's listener (the stand-in engine hears "hey
/// coder", `coder`'s phrase), and the window switches to `coder` and
/// answers the request it records in a new session there.
#[test]
fn a_wake_phrase_of_another_profile_switches_the_desktop_to_it() {
    let home = mock_home();
    mock_profile(home.path(), "coder");
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    for profile in ["default", "coder"] {
        cloud_speech(home.path(), profile, LISTEN);
        append(home.path(), profile, "\n[wake]\nenabled = true\n");
    }
    append(home.path(), "coder", "phrase = \"hey coder\"\n");
    let audio = home.path().join("audio");
    std::fs::create_dir_all(&audio).unwrap();
    std::fs::write(audio.join("wake"), "hey coder\ncoder\n").unwrap();
    let setup = Setup {
        env: vec![(
            "PMT_TEST_AUDIO_DIR".to_owned(),
            audio.to_string_lossy().into_owned(),
        )],
        ..Setup::default()
    };
    drive(home.path(), "desktop-wake.spec.ts", setup, &[]);
}
