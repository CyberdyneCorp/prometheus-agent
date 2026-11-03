//! `prometheus gateway whatsapp pair` against a stand-in bridge (spec:
//! platform-adapters, `WhatsApp` pairing and modes): the QR code is drawn
//! until the phone links, a pairing code replaces it on request, a narrow
//! terminal is refused, an earlier session is replaced, and pairing that
//! never completes exits 75.

#![cfg(all(unix, feature = "gateway"))]

mod common;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use common::{command, temp_home};

/// A bridge that records its arguments and prints `events` as JSON lines.
fn fake_bridge(home: &Path, events: &[&str]) -> std::path::PathBuf {
    let script = home.join("fake-bridge.sh");
    let mut body = String::from("#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/bridge-args\"\n");
    for event in events {
        let _ = writeln!(body, "echo '{event}'\nsleep 0.1");
    }
    // A linked bridge exits; one still pairing waits (and is killed).
    if events.iter().any(|e| e.contains("connected")) {
        body.push_str("exit 0\n");
    } else {
        body.push_str("exec sleep 30\n");
    }
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn pair(home: &Path, bridge: &Path, columns: &str, extra: &[&str]) -> std::process::Output {
    command(home)
        .args(["gateway", "whatsapp", "pair"])
        .args(extra)
        .env("PMT_TEST_WHATSAPP_BRIDGE", bridge)
        .env("PMT_TEST_WHATSAPP_PAIR_SECS", "3")
        .env("COLUMNS", columns)
        .output()
        .unwrap()
}

fn session(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/platforms/whatsapp/session")
}

#[test]
fn the_qr_code_is_drawn_until_the_phone_links_and_replaces_the_old_session() {
    let home = temp_home();
    let home = home.path();
    std::fs::create_dir_all(session(home)).unwrap();
    std::fs::write(session(home).join("creds.json"), "old").unwrap();
    let bridge = fake_bridge(
        home,
        &[
            r#"{"event":"started"}"#,
            r#"{"event":"qr","qr":"2@first,abc"}"#,
            r#"{"event":"qr","qr":"2@second,def"}"#,
            r#"{"event":"connected","user":{"id":"551199@s.whatsapp.net","name":"Ana"}}"#,
        ],
    );
    let out = pair(home, &bridge, "100", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains('▀') || stdout.contains('█'),
        "a QR code: {stdout}"
    );
    assert!(
        stdout.matches("Linked devices").count() >= 2,
        "refreshed: {stdout}"
    );
    assert!(stdout.contains("linked Ana"), "{stdout}");
    let args = std::fs::read_to_string(home.join("bridge-args")).unwrap();
    assert!(args.contains("--pair-only --pair-json --session"), "{args}");
    assert!(args.contains("--mode bot"), "{args}");
    assert!(
        !session(home).join("creds.json").exists(),
        "the earlier session is replaced"
    );
    let mode = std::fs::metadata(session(home))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn a_narrow_terminal_is_refused_and_a_pairing_code_needs_no_qr() {
    let home = temp_home();
    let home = home.path();
    let bridge = fake_bridge(
        home,
        &[
            r#"{"event":"pairing_code","code":"ABCD-EFGH"}"#,
            r#"{"event":"connected","user":{"name":"Ana"}}"#,
        ],
    );
    let narrow = pair(home, &bridge, "40", &[]);
    assert_eq!(narrow.status.code(), Some(2));
    let err = String::from_utf8_lossy(&narrow.stderr);
    assert!(
        err.contains("40 columns") && err.contains("--pairing-code"),
        "{err}"
    );
    let coded = pair(
        home,
        &bridge,
        "40",
        &["--pairing-code", "+55 11 99999-9999"],
    );
    let stdout = String::from_utf8_lossy(&coded.stdout);
    assert_eq!(coded.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("Pairing code: ABCD-EFGH"), "{stdout}");
    let args = std::fs::read_to_string(home.join("bridge-args")).unwrap();
    assert!(args.contains("--pairing-code +55 11 99999-9999"), "{args}");
}

#[test]
fn pairing_that_never_completes_exits_75() {
    let home = temp_home();
    let home = home.path();
    let bridge = fake_bridge(home, &[r#"{"event":"qr","qr":"2@only,abc"}"#]);
    let out = pair(home, &bridge, "100", &[]);
    assert_eq!(out.status.code(), Some(75));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not linked within"));
}
