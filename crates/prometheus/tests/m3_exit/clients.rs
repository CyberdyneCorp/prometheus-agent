//! Task 81.1: the desktop app and the dashboard attach to one daemon and
//! show the same live session, and an ACP editor completes a prompt round
//! trip on that same daemon (spec: desktop-app, Daemon attachment through
//! the bundled binary; web-dashboard, Structured chat over the client
//! protocol; acp-adapter, Daemon-backed sessions). The browser leg runs
//! `web/e2e/m3-exit-clients.spec.ts` against the desktop browser harness;
//! the editor leg is the ACP fixture of `tests/acp`.

use std::path::Path;

use serde_json::{Value, json};

use crate::acp_fixture::Acp;
use crate::common::dashboard::playwright;
use crate::common::{daemon, mock_home, ok, run};

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

fn sessions(home: &Path) -> Vec<Value> {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    listed["sessions"].as_array().unwrap().clone()
}

#[test]
fn desktop_dashboard_and_an_acp_editor_share_one_daemon() {
    let home = mock_home();
    let _stop = Stop(home.path());
    let harness = crate::common::desktop::start(home.path());
    playwright(
        home.path(),
        "m3-exit-clients.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
    let desktop_session = std::fs::read_to_string(home.path().join("m3-exit-session")).unwrap();
    let pid = daemon::record(home.path()).process.pid;

    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&work.canonicalize().unwrap());
    let id = acp.send_prompt(&session, "hello from the editor");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(
        acp.agent_text(&session)
            .contains("mock: hello from the editor"),
        "{:?}",
        acp.seen
    );
    acp.assert_only_frames();

    assert_eq!(
        daemon::record(home.path()).process.pid,
        pid,
        "the editor used the daemon the desktop started"
    );
    let listed = sessions(home.path());
    let source = |id: &str| {
        listed
            .iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("session {id} not in {listed:?}"))["source"]
            .clone()
    };
    assert_eq!(source(&desktop_session), "desktop");
    assert_eq!(source(&session), "acp");
}
