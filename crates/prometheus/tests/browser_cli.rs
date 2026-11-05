//! `prometheus browser` and `browser.*` configuration through the binary
//! (spec: execution-environments, First-party cloud browser providers;
//! Connecting to the user's browser; Browser session resolution).

mod common;

use common::{ok, run, stderr, stdout, temp_home};
use pmt_tools::browser::status::{Entry, Sessions};
use serde_json::Value;

fn cloud_session(cache: &std::path::Path) {
    Sessions::new(cache).write(Entry {
        key: "ses_a/default".to_owned(),
        origin: "cloud".to_owned(),
        provider: Some("browserbase".to_owned()),
        endpoint: None,
        pid: None,
        profile: None,
        headed: false,
        fallback_from_cloud: false,
        owner_pid: 0,
        owner_start: 0,
    });
}

#[test]
fn status_lists_live_sessions_with_their_provider() {
    let home = temp_home();
    let out = run(home.path(), &["browser", "status"]);
    let text = ok(&out);
    assert!(text.contains("provider: local"), "{text}");
    assert!(text.contains("no live browser sessions"), "{text}");

    // A session of a running process (this test) using Browserbase.
    cloud_session(&home.path().join("profiles/default/cache/browser"));
    let text = ok(&run(
        home.path(),
        &["--set", "browser.provider=browserbase", "browser", "status"],
    ));
    assert!(text.contains("provider: browserbase"), "{text}");
    assert!(
        text.contains("ses_a/default  cloud  provider=browserbase"),
        "{text}"
    );
    let json: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "browser", "status"]))).unwrap();
    assert_eq!(json["sessions"][0]["provider"], "browserbase");
}

#[test]
fn first_party_providers_are_accepted_and_unknown_ones_refused() {
    let home = temp_home();
    for provider in [
        "browserbase",
        "browser-use",
        "firecrawl",
        "subscription",
        "local",
    ] {
        ok(&run(
            home.path(),
            &["config", "set", "browser.provider", provider],
        ));
        let out = run(home.path(), &["config", "validate"]);
        assert!(out.status.success(), "{provider}: {}", stderr(&out));
    }
    ok(&run(
        home.path(),
        &["config", "set", "browser.provider", "nope"],
    ));
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("unknown browser provider 'nope'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn close_profile_needs_confirmation_when_not_on_a_terminal() {
    let home = temp_home();
    let out = run(home.path(), &["browser", "close-profile"]);
    // Without a usable default browser it says why; with one, it refuses
    // to end it without --yes. It never ends anything here.
    let code = out.status.code();
    assert!(
        code == Some(1) || code == Some(4),
        "{code:?} {}",
        stderr(&out)
    );
    assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));
}

#[test]
fn the_connected_browser_tools_are_offered_only_with_an_endpoint() {
    let home = temp_home();
    let row = |text: &str, tool: &str| {
        text.lines()
            .find(|line| line.starts_with(tool))
            .unwrap_or_default()
            .to_owned()
    };
    let text = ok(&run(home.path(), &["tools", "list"]));
    for tool in ["browser_cdp", "browser_dialog"] {
        assert!(
            row(&text, tool).contains("unavailable:no browser connected"),
            "{text}"
        );
    }
    let text = ok(&run(
        home.path(),
        &[
            "--set",
            "browser.cdp_url=ws://127.0.0.1:9/devtools/browser/x",
            "tools",
            "list",
        ],
    ));
    for tool in ["browser_cdp", "browser_dialog"] {
        assert!(row(&text, tool).contains("exposed"), "{text}");
    }
}
