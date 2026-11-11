//! UI languages on the messaging gateway inside the real daemon (spec:
//! i18n, display.language selection; Language changes apply without
//! restart (upgrade)), driven through the debug-build file adapter.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use self::common::gateway::{Daemon, gateway_home, send, texts, wait_said};
use self::common::{ok, run};

/// A profile answering on the `test` platform in `language`.
fn profile_in(home: &Path, name: &str, language: &str) {
    ok(&run(home, &["profile", "create", name]));
    let config = home.join("profiles").join(name).join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap_or_default();
    text.push_str(
        "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n\
         [gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    std::fs::write(&config, text).unwrap();
    ok(&run(
        home,
        &["-p", name, "config", "set", "display.language", language],
    ));
}

/// Scenario: Two profiles, two languages.
#[test]
fn each_profile_on_one_gateway_answers_status_in_its_own_language() {
    let home = gateway_home("");
    let home = home.path();
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    profile_in(home, "a", "es");
    profile_in(home, "b", "ja");
    let _daemon = Daemon::start(home);
    for profile in ["a", "b"] {
        send(home, profile, "u1", "hello");
        wait_said(home, profile, "mock: hello");
        send(home, profile, "u1", "/status");
    }
    wait_said(home, "a", "proveedor");
    wait_said(home, "b", "プロバイダー");
    let status_a = texts(home, "a")
        .into_iter()
        .find(|t| t.contains("proveedor"))
        .unwrap();
    assert!(status_a.contains("modelo"), "{status_a}");
    let status_b = texts(home, "b")
        .into_iter()
        .find(|t| t.contains("プロバイダー"))
        .unwrap();
    assert!(
        status_b.contains("モデル") && !status_b.contains("proveedor"),
        "{status_b}"
    );
}

/// Scenario: Gateway picks up an overlay edit.
#[test]
fn an_overlay_edit_shows_in_the_next_notice_without_a_restart() {
    let home = gateway_home("");
    let home = home.path();
    let _daemon = Daemon::start(home);
    send(home, "default", "u1", "/stop");
    wait_said(home, "default", "No active task to stop.");
    let locales = home.join("profiles/default/locales");
    std::fs::create_dir_all(&locales).unwrap();
    std::fs::write(
        locales.join("en.toml"),
        "[gateway]\nnothing_to_stop = \"Nothing is running here.\"\n",
    )
    .unwrap();
    let edited = Instant::now();
    loop {
        send(home, "default", "u1", "/stop");
        std::thread::sleep(Duration::from_millis(500));
        if texts(home, "default")
            .iter()
            .any(|t| t == "Nothing is running here.")
        {
            break;
        }
        assert!(
            edited.elapsed() < Duration::from_secs(5),
            "not within 5 s: {:?}",
            texts(home, "default")
        );
    }
}
