//! Running daemon handover (spec: distribution-and-update, Running daemon
//! handover): after the swap, the daemon of the replaced binary restarts
//! on the new build once its turns are done; a turn still running at
//! `update.drain_timeout_secs` keeps it on the old build, the update still
//! succeeds and the receipt lists `daemon_restart_pending`.
#![cfg(feature = "gateway")]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::release::{Fixture, code, next_patch};
use pmt_update::Channel;

/// A self install with the mock provider and a newer release published.
fn fixture() -> Fixture {
    let f = Fixture::self_install();
    common::ok(&f.run(&["config", "set", "model.provider", "mock"]));
    common::ok(&f.run(&["config", "set", "model.default", "mock"]));
    f.publish(&next_patch(f.version), Channel::Stable, &f.bytes(), None);
    f
}

fn daemon_status(f: &Fixture) -> serde_json::Value {
    let out = common::ok(&f.run(&["--json", "daemon", "status"]));
    serde_json::from_str(&out).unwrap()
}

fn instance(f: &Fixture) -> String {
    daemon_status(f)["daemon"]["instance_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// A daemon of the installed binary, stopped on drop.
struct Running<'a>(&'a Fixture);

impl<'a> Running<'a> {
    fn start(f: &'a Fixture) -> Self {
        common::ok(&f.run(&["daemon", "start"]));
        Self(f)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["daemon", "stop"]);
    }
}

/// A one-shot chat whose turn takes 20 s, attached to the daemon.
fn slow_turn(f: &Fixture) -> Child {
    let script = f.home.join("slow.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    )
    .unwrap();
    let mut cmd = Command::new(&f.binary);
    common::isolate_user_home(&mut cmd, &f.home);
    let child = cmd
        .arg("--home")
        .arg(&f.home)
        .args(["--mock-script", script.to_str().unwrap(), "-z", "wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_mins(1);
    while daemon_status(f)["daemon"]["active_turns"].as_u64() != Some(1) {
        assert!(Instant::now() < deadline, "the slow turn never started");
        std::thread::sleep(Duration::from_millis(100));
    }
    child
}

#[test]
fn an_idle_daemon_restarts_on_the_new_build() {
    let f = fixture();
    let _daemon = Running::start(&f);
    let before = instance(&f);
    let out = f.run(&["update"]);
    let text = common::ok(&out);
    assert!(
        text.contains(&format!(
            "daemon restarted on {}",
            f.version.to_string().trim_start_matches('v')
        )),
        "{text}"
    );
    let stages = f.latest_receipt()["stages"].clone();
    assert!(
        stages.as_array().unwrap().iter().any(|s| s == "restart"),
        "{stages}"
    );
    let after = instance(&f);
    assert!(!after.is_empty() && after != before, "a new daemon runs");
}

#[test]
fn a_busy_daemon_keeps_its_turn_and_the_restart_stays_pending() {
    let f = fixture();
    f.daemon("drain_timeout_secs = 1\n");
    let _daemon = Running::start(&f);
    let before = instance(&f);
    let mut chat = slow_turn(&f);
    let out = f.run(&["update", "--yes"]);
    let text = common::ok(&out);
    assert!(
        text.contains("restart it with: prometheus daemon restart"),
        "{text}"
    );
    let receipt = f.latest_receipt();
    assert_eq!(receipt["outcome"], "success");
    assert_eq!(
        receipt["follow_ups"],
        serde_json::json!(["daemon_restart_pending"])
    );
    assert_eq!(instance(&f), before, "the daemon kept running");
    assert_eq!(
        daemon_status(&f)["daemon"]["active_turns"],
        1,
        "its turn too"
    );
    let _ = chat.kill();
    let _ = chat.wait();
}

#[test]
fn restarting_a_busy_daemon_needs_a_yes_without_a_terminal() {
    let f = fixture();
    let _daemon = Running::start(&f);
    let mut chat = slow_turn(&f);
    let before = f.bytes();
    let out = f.run(&["update"]);
    assert_eq!(code(&out), 4, "{}", common::stderr(&out));
    assert!(
        common::stderr(&out).contains("running turn(s)"),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(f.bytes(), before, "nothing was swapped");
    let _ = chat.kill();
    let _ = chat.wait();
}
