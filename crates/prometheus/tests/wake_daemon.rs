//! The desktop's ear on the daemon (spec: voice-mode, Wake capture
//! location for remote clients; Wake word profile routing; Single wake
//! listener ownership across surfaces), against a daemon whose host has
//! no microphone (the debug-build `PMT_TEST_AUDIO_DIR` seam without a
//! `device`) and a client streaming recorded audio through `wake.feed`.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use common::{ok, run, temp_home};
use pmt_rpc::contracts::{
    WakeArm, WakeArmParams, WakeFeed, WakeFeedParams, WakeProfileParams, WakeStatus,
};
use serde_json::Value;

/// A mock home that speaks and hears through command providers; the
/// seam directory names the detection the stand-in engine reports.
fn home_with(detection: &str) -> (tempfile::TempDir, String) {
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
        + "\n[stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
           command = \"printf 'x'\"\n\n[tts]\nprovider = \"fake\"\n\n\
           [tts.providers.fake]\ntype = \"command\"\ncommand = \"cp {input_path} {output_path}\"\n";
    std::fs::write(&config, text).unwrap();
    let audio = home.path().join("audio");
    std::fs::create_dir_all(&audio).unwrap();
    // No `device`: the daemon's host has no microphone.
    std::fs::write(audio.join("wake"), detection).unwrap();
    let audio = audio.to_string_lossy().into_owned();
    (home, audio)
}

fn speech() -> Vec<u8> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-voice/tests/fixtures/question.wav");
    let wav = std::fs::read(path).unwrap();
    // The 44-byte header off: 16 kHz mono 16-bit PCM.
    wav[44..].to_vec()
}

async fn detected(inbox: &mut pmt_rpc::client::Inbox) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("wake.detected")
            .expect("connection open");
        if event.event == "wake.detected" {
            return event.data;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_daemon_detects_the_wake_phrase_in_frames_the_desktop_sends() {
    let (home, audio) = home_with("hey prometheus\n");
    let _daemon = common::daemon::start_env(home.path(), &[("PMT_TEST_AUDIO_DIR", &audio)]);
    let (client, mut inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    let armed = client
        .call_typed::<WakeArm>(&WakeArmParams {
            profile: "default".to_owned(),
            client_capture: true,
        })
        .await
        .unwrap();
    assert_eq!(armed.capture, "client");
    assert_eq!(armed.phrase, "hey prometheus");
    let status = client
        .call_typed::<WakeStatus>(&WakeProfileParams {
            profile: "default".to_owned(),
        })
        .await
        .unwrap();
    assert!(status.armed);
    assert!(
        status.report.contains("owner: this desktop app"),
        "{}",
        status.report
    );
    assert!(
        status
            .report
            .contains("input device: the client's microphone"),
        "{}",
        status.report
    );
    for chunk in speech().chunks(3_200) {
        client
            .call_typed::<WakeFeed>(&WakeFeedParams {
                profile: "default".to_owned(),
                pcm: base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .await
            .unwrap();
    }
    let event = detected(&mut inbox).await;
    assert_eq!(event["profile"], "default");
    assert_eq!(event["phrase"], "hey prometheus");
    // The ear's lock is the daemon's on behalf of the desktop; it goes
    // when the connection does.
    assert!(home.path().join("run/wake.lock").exists());
    client.close();
    drop(inbox);
    for _ in 0..50 {
        if !home.path().join("run/wake.lock").exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the wake lock outlived its client");
}

#[tokio::test(flavor = "multi_thread")]
async fn another_profiles_phrase_wakes_that_profile() {
    let (home, audio) = home_with("hey coder\ncoder\n");
    ok(&run(home.path(), &["profile", "create", "coder"]));
    let _daemon = common::daemon::start_env(home.path(), &[("PMT_TEST_AUDIO_DIR", &audio)]);
    let (client, mut inbox) = common::daemon::desktop(home.path(), "default", &[]).await;
    client
        .call_typed::<WakeArm>(&WakeArmParams {
            profile: "default".to_owned(),
            client_capture: true,
        })
        .await
        .unwrap();
    for chunk in speech().chunks(3_200) {
        client
            .call_typed::<WakeFeed>(&WakeFeedParams {
                profile: "default".to_owned(),
                pcm: base64::engine::general_purpose::STANDARD.encode(chunk),
            })
            .await
            .unwrap();
    }
    let event = detected(&mut inbox).await;
    assert_eq!(event["profile"], "coder");
    assert_eq!(event["phrase"], "hey coder");
}
