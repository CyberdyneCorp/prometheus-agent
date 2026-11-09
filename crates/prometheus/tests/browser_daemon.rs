//! `/browser connect` in a daemon serving several sessions (spec:
//! execution-environments, `browser_cdp` and `browser_dialog` tools;
//! Connecting to the user's browser).

#![cfg(all(unix, feature = "gateway"))]

mod common;

use common::{daemon, mock_home, ok, pty, run};
use expectrl::Expect as _;

/// A DevTools endpoint nothing listens on: `/browser connect` takes a
/// `ws://` URL as given, so no browser is needed to offer the tools.
const ENDPOINT: &str = "ws://127.0.0.1:9/devtools/browser/fake";

fn new_session(home: &std::path::Path) -> String {
    let report: serde_json::Value =
        serde_json::from_str(&ok(&run(home, &["--json", "-z", "first"]))).unwrap();
    report["session_id"].as_str().unwrap().to_owned()
}

/// The `tools list --session` rows of the connected-browser tools.
fn cdp_rows(home: &std::path::Path, session: &str) -> Vec<String> {
    let text = ok(&run(home, &["tools", "list", "--session", session]));
    ["browser_cdp", "browser_dialog"]
        .iter()
        .map(|tool| {
            text.lines()
                .find(|line| line.starts_with(tool))
                .unwrap_or_else(|| panic!("{tool} missing: {text}"))
                .to_owned()
        })
        .collect()
}

fn exposed(home: &std::path::Path, session: &str) -> bool {
    let rows = cdp_rows(home, session);
    let offered = rows.iter().filter(|row| row.contains(" exposed")).count();
    assert!(offered == 0 || offered == rows.len(), "{rows:?}");
    offered == rows.len()
}

/// Scenario: Connect offers the CDP tools to that session only. Session A
/// connects in the daemon and gets `browser_cdp`/`browser_dialog` in its
/// tool array; session B, built in the same process while A is
/// connected, does not; `/browser disconnect` takes them away from A.
#[test]
fn connect_offers_the_cdp_tools_to_the_connecting_session_only() {
    let home = mock_home();
    let home = home.path();
    let _daemon = daemon::start(home, &[]);
    let a = new_session(home);
    assert!(!exposed(home, &a));

    let mut p = pty::spawn(home, &["-r", &a]);
    p.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut p);
    p.send_line(format!("/browser connect {ENDPOINT}")).unwrap();
    p.expect("browser_cdp and browser_dialog are available now")
        .unwrap();
    pty::prompt(&mut p);
    assert!(exposed(home, &a), "A connected");

    let b = new_session(home);
    assert!(!exposed(home, &b), "B is not connected");
    let row = &cdp_rows(home, &b)[0];
    assert!(row.contains("unavailable:no browser connected"), "{row}");

    p.send_line("/browser disconnect").unwrap();
    p.expect("disconnected").unwrap();
    pty::prompt(&mut p);
    assert!(!exposed(home, &a), "A disconnected");
    pty::exit(p);
}
