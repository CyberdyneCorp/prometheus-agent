//! Real-profile browsing with a stand-in "real" profile: the session
//! launches the user's default browser on a managed snapshot, never on the
//! live profile (spec: execution-environments, Real-profile browsing;
//! Local browser isolation).

use std::path::Path;
use std::sync::Arc;

use pmt_core::capability::{Capability, CapabilitySet};
use pmt_tools::browser::real_profile::{Browser, Detect};
use pmt_tools::result::Status;

use crate::support::{self, Fixture, run, run_local};

struct Fixed(Browser);

impl Detect for Fixed {
    fn default_browser(&self) -> Result<Browser, String> {
        Ok(self.0.clone())
    }
}

fn granted() -> CapabilitySet {
    std::iter::once(Capability::BrowserRealProfile).collect()
}

/// A user's browser data directory with a `Default` and a `Work` profile.
fn real_browser(home: &Path) -> Browser {
    let data = home.join("real-browser");
    for profile in ["Default", "Work"] {
        std::fs::create_dir_all(data.join(profile)).unwrap();
        std::fs::write(data.join(profile).join("Preferences"), "{}").unwrap();
    }
    std::fs::write(
        data.join("Local State"),
        r#"{"profile":{"last_used":"Work"}}"#,
    )
    .unwrap();
    Browser {
        name: "testbrowser".to_owned(),
        executable: support::browsers()[0].clone(),
        user_data_dir: data,
    }
}

fn fixture_with_real_profile() -> (Fixture, Browser) {
    let mut fixture = Fixture::new();
    let browser = real_browser(&fixture.home);
    fixture.detect = Arc::new(Fixed(browser.clone()));
    fixture.config.lock().unwrap().use_real_profile = true;
    (fixture, browser)
}

#[tokio::test]
async fn a_granted_session_browses_a_snapshot_of_the_last_used_profile() {
    let (fixture, browser) = fixture_with_real_profile();
    let manager = fixture.manager();
    let exec = fixture.exec_granted(&manager, false, granted());
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert!(body.get("real_profile_unavailable").is_none(), "{body}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.profile(), Some("real:testbrowser"));
    let snapshot = fixture.browser_cache().join("real-profile/testbrowser");
    assert!(snapshot.join("Work/Preferences").is_file());
    assert!(
        !snapshot.join("Default").exists(),
        "only the browsed profile"
    );
    // Launched on the snapshot, never on the live directory.
    let command = command_line(session.pid().unwrap());
    let live = format!("--user-data-dir={}", browser.user_data_dir.display());
    let copy = format!("--user-data-dir={}", snapshot.display());
    assert!(command.contains(&copy), "{command:?}");
    assert!(!command.contains(&live), "{command:?}");
    assert!(
        command.iter().any(|a| a == "--profile-directory=Work"),
        "{command:?}"
    );
    assert!(
        command.iter().any(|a| a.starts_with("--headless")),
        "{command:?}"
    );
    drop(session);
    manager.shutdown().await;
    assert!(
        snapshot.is_dir(),
        "the snapshot is kept for the next launch"
    );
}

/// Scenario: Missing pin.
#[tokio::test]
async fn a_missing_pin_gives_a_throwaway_profile_and_says_why() {
    let (fixture, _) = fixture_with_real_profile();
    fixture.config.lock().unwrap().real_profile_pin = "Profile 9".to_owned();
    let manager = fixture.manager();
    let exec = fixture.exec_granted(&manager, false, granted());
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let why = body["real_profile_unavailable"].as_str().unwrap();
    assert!(why.contains("Profile 9"), "{why}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.profile(), Some("throwaway"));
    drop(session);
    let (_, later) = run(&exec, "print(1)").await;
    assert!(
        later.get("real_profile_unavailable").is_none(),
        "only the first result"
    );
    manager.shutdown().await;
}

/// Scenario: Revoking consent.
#[tokio::test]
async fn turning_real_profile_browsing_off_deletes_the_snapshot_at_the_next_session() {
    let (fixture, _) = fixture_with_real_profile();
    let manager = fixture.manager();
    let exec = fixture.exec_granted(&manager, false, granted());
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let snapshot = fixture.browser_cache().join("real-profile/testbrowser");
    assert!(snapshot.is_dir());
    manager.shutdown().await;

    fixture.config.lock().unwrap().use_real_profile = false;
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert!(!fixture.browser_cache().join("real-profile").exists());
    manager.shutdown().await;
}

#[tokio::test]
async fn without_the_grant_the_session_gets_a_throwaway_profile() {
    let (fixture, _) = fixture_with_real_profile();
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let (result, body) = run_local(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.profile(), Some("throwaway"));
    drop(session);
    assert!(!fixture.browser_cache().join("real-profile").exists());
    manager.shutdown().await;
}

fn command_line(pid: u32) -> Vec<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .process(pid)
        .map(|p| {
            p.cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}
