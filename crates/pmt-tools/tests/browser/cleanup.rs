//! A browser is killed together with its helper processes, and its profile
//! directory is gone even when a helper was still writing to it (spec:
//! execution-environments, Browser supervision, dialogs and cleanup).
//!
//! A fake browser stands in for Chromium: it publishes a DevTools endpoint
//! and forks a helper that keeps recreating files in the profile, the way
//! Chromium's zygote, renderer and crashpad helpers do on Linux.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_tools::browser::launch::{Launched, launch};
use pmt_tools::browser::records::{Record, Records};

use crate::support::Fixture;

/// The fake browser; it writes its helper's pid to `$HOME/helper.pid`.
const FAKE_BROWSER: &str = r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in --user-data-dir=*) dir="${arg#--user-data-dir=}" ;; esac
done
(while :; do mkdir -p "$dir/Default" && : > "$dir/Default/Cookies-journal"; sleep 0.01; done) &
echo $! > "$HOME/helper.pid"
printf '1\n/devtools/browser/fake\n' > "$dir/DevToolsActivePort"
sleep 600
"#;

async fn launch_fake(fixture: &Fixture) -> Launched {
    let fake = fixture.home.join("fake-browser");
    std::fs::write(&fake, FAKE_BROWSER).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    launch(
        &fake,
        &fixture.browser_cache().join("profiles"),
        &fixture.env(),
        Duration::from_secs(10),
    )
    .await
    .unwrap()
}

/// The helper's pid; on drop the helper is killed, so a failing test does
/// not leave it looping.
struct Helper(String);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &self.0])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn helper_pid(fixture: &Fixture) -> Helper {
    let file = fixture.home.join("helper.pid");
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(&file)
            && !text.trim().is_empty()
        {
            return Helper(text.trim().to_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the fake browser never started its helper");
}

fn is_running(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Waits briefly for `pid` to die (an orphaned helper is reaped by init).
fn gone(pid: &str) -> bool {
    for _ in 0..100 {
        if !is_running(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The profile directory stays gone (no helper recreates it).
fn stays_gone(dir: &Path) -> bool {
    std::thread::sleep(Duration::from_millis(200));
    !dir.exists()
}

#[tokio::test]
async fn terminating_a_browser_kills_its_helpers_and_removes_the_profile() {
    let fixture = Fixture::new();
    let launched = launch_fake(&fixture).await;
    let helper = helper_pid(&fixture);
    let (profile_dir, temp_dir): (PathBuf, PathBuf) =
        (launched.profile_dir.clone(), launched.temp_dir.clone());
    launched.terminate().await;
    assert!(gone(&helper.0), "the helper outlived its browser");
    assert!(stays_gone(&profile_dir), "the profile directory survived");
    assert!(!temp_dir.exists());
}

#[tokio::test]
async fn reaping_an_orphan_kills_its_helpers_and_removes_the_profile() {
    let fixture = Fixture::new();
    let mut launched = launch_fake(&fixture).await;
    let helper = helper_pid(&fixture);
    let gone_owner = std::process::Command::new("true").spawn().unwrap();
    let owner_pid = gone_owner.id();
    let _ = gone_owner.wait_with_output();
    let record = Record {
        owner_pid,
        owner_start: 1,
        browser_pid: launched.pid,
        browser_start: pmt_core::runtime::process_start_time(launched.pid).unwrap(),
        profile_dir: launched.profile_dir.clone(),
        temp_dir: Some(launched.temp_dir.clone()),
    };
    let records_dir = fixture.browser_cache().join("records");
    std::fs::create_dir_all(&records_dir).unwrap();
    std::fs::write(
        records_dir.join("orphan.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();

    assert_eq!(Records::new(&fixture.browser_cache()).reap_orphans(), 1);
    let exited = tokio::time::timeout(Duration::from_secs(10), launched.child.wait()).await;
    assert!(exited.is_ok(), "the orphaned browser was killed");
    assert!(gone(&helper.0), "the helper outlived its browser");
    assert!(
        stays_gone(&launched.profile_dir),
        "the profile directory survived"
    );
    assert!(!launched.temp_dir.exists());
}
