//! Update from chat in the real daemon (spec: distribution-and-update,
//! Update from chat): an admin's confirmed `/update` runs the updater
//! detached, its stages reach the chat, the daemon is handed over to the
//! new build, and the new daemon says `updated to <version>`.
#![cfg(feature = "gateway")]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::gateway::{adapter_dir, send, texts, wait, wait_said, wait_said_within};
use common::release::{Fixture, next_patch};
use pmt_rpc::discovery::{Discovery, discover};
use pmt_update::Channel;

/// The installed binary's foreground daemon with the file adapter.
fn daemon(f: &Fixture) -> Child {
    let mut cmd = Command::new(&f.binary);
    common::isolate_user_home(&mut cmd, &f.home);
    let child = cmd
        .arg("--home")
        .arg(&f.home)
        .args(["daemon", "run"])
        .env("PMT_TEST_ADAPTER_DIR", adapter_dir(&f.home))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait("the daemon runs", Duration::from_secs(30), || {
        matches!(discover(&f.home), Ok(Discovery::Running(_)))
    });
    child
}

#[test]
fn an_admin_updates_from_the_chat() {
    let f = Fixture::self_install();
    common::ok(&f.run(&["config", "set", "model.provider", "mock"]));
    common::ok(&f.run(&["config", "set", "model.default", "mock"]));
    common::append_config(
        &f.home,
        "\n[gateway.busy]\ndebounce_ms = 0\n\n[gateway.platforms.test]\nenabled = true\nallow_all = true\nadmins = [\"boss\"]\n",
    );
    f.daemon("allow_from_chat = true\n");
    let next = next_patch(f.version);
    f.publish(&next, Channel::Stable, &f.bytes(), None);
    let mut first = daemon(&f);

    send(&f.home, "default", "boss", "/update");
    wait_said(&f.home, "default", "Update prometheus now?");
    send(&f.home, "default", "boss", "/approve");
    wait_said(&f.home, "default", "Update started");
    wait_said(&f.home, "default", "checking the stable channel");
    // Download, self-test, swap and handover: minutes on the slow Intel
    // macOS runner (a ~100 MB debug-build tarball).
    wait_said_within(
        &f.home,
        "default",
        &format!("updated to {next}"),
        Duration::from_mins(4),
    );
    // Stage lines, at most one per 2 s: a newer stage replaces a waiting
    // one, so which middle stages show depends on timing. The last one,
    // written just before the handover stops this daemon, always shows.
    let said = texts(&f.home, "default");
    let at = |line: &str| {
        said.iter()
            .position(|t| t == line)
            .unwrap_or_else(|| panic!("never said {line:?}: {said:?}"))
    };
    assert!(at("checking the stable channel") < at("restarting the daemon"));
    assert!(at("restarting the daemon") < at(&format!("updated to {next}")));
    assert_eq!(f.latest_receipt()["outcome"], "success");
    // The first daemon exited for the handover; the new one runs.
    assert!(first.wait().is_ok());
    let _ = f.run(&["daemon", "stop"]);
}
