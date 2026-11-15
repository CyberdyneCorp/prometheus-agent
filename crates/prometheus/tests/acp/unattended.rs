//! Task 66.5 ACP fixtures: optional browser tools and unattended clients
//! with relay bridges (spec: acp-adapter).

use std::time::Duration;

use serde_json::{Value, json};

use super::common::{self, daemon};
use super::fixture::{Acp, Home};

fn exposed(listing: &str, tool: &str) -> bool {
    listing
        .lines()
        .any(|l| l.split_whitespace().next() == Some(tool) && l.contains(" exposed"))
}

/// A bridge that approves every card it is shown.
fn approving() -> super::fixture::Responder {
    Box::new(|request: &Value| {
        if request["method"] == "session/request_permission" {
            Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}))
        } else {
            Err(json!({"code": -32601, "message": "not offered"}))
        }
    })
}

/// A relay setup that does give unattended sessions the terminal, so an
/// approval (not a missing tool) decides whether `rm -rf build/` runs.
fn rm_script(home: &Home) {
    home.config("\n[toolsets.surfaces]\nacp_unattended = [\"acp\"]\n");
    home.script(
        "[[rule]]\nuser = \"clean\"\n[[rule.tool_calls]]\nid = \"call-rm\"\nname = \"terminal\"\narguments = { command = \"rm -rf build/\" }\n\n[[rule]]\ntext = \"cleaned\"\nrepeat = true\n",
    );
}

/// The installed version directory the pinned `chromium` of this host
/// has (`None` where no build is pinned).
fn chromium_dir() -> Option<String> {
    let index = pmt_update::runtimes::index::RuntimeIndex::embedded();
    let pin = index.pin("chromium").unwrap();
    let artifact = pin.artifacts.get(&pmt_update::runtimes::platform())?;
    Some(pin.dir_name(artifact))
}

/// Scenario: Runtime not installed; once the managed Chromium is
/// installed the check passes and `--setup-browser` has nothing to do.
#[test]
fn browser_tools_need_the_managed_chromium_at_session_creation() {
    let home = Home::new();
    home.config("\n[toolsets.surfaces]\nacp = [\"acp\", \"browser\"]\n");
    let out = common::run(home.path(), &["acp", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        common::stdout(&out).trim(),
        "browser runtime not installed; run 'prometheus acp --setup-browser'"
    );
    let refused = common::run(home.path(), &["acp", "--setup-browser"]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "{}",
        common::stderr(&refused)
    );
    assert!(common::stderr(&refused).contains("--yes"));
    {
        let _daemon = daemon::start(home.path(), &[]);
        let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
        acp.initialize(&json!({}));
        let session = acp.new_session(&home.work);
        let listing = common::ok(&common::run(
            home.path(),
            &["tools", "list", "--session", &session],
        ));
        assert!(
            !listing
                .lines()
                .any(|l| l.starts_with("browser_") && l.contains(" exposed")),
            "{listing}"
        );
    }
    let Some(dir) = chromium_dir() else {
        return;
    };
    common::fake_runtime(home.path(), "chromium", &dir, &[("chrome/chrome", b"exe")]);
    let out = common::run(home.path(), &["acp", "--check"]);
    assert_eq!(common::stdout(&out).trim(), "Prometheus ACP check OK");
    let again = common::ok(&common::run(
        home.path(),
        &["acp", "--setup-browser", "--yes"],
    ));
    assert!(
        again.starts_with("chromium is already installed at "),
        "{again}"
    );
}

/// Scenario: Relay bridge auto-approves. A bridge that would approve
/// everything is never asked; the command is blocked by the default
/// `approvals.unattended_mode = "deny"`.
#[test]
fn an_unattended_bridge_is_never_asked_and_the_command_is_blocked() {
    let home = Home::new();
    rm_script(&home);
    std::fs::create_dir_all(home.work.join("build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &["--unattended"], &[]);
    acp.initialize(&json!({}));
    acp.respond_with(approving());
    let session = acp.new_session(&home.work);
    let id = acp.send_prompt(&session, "clean");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(
        acp.requests("session/request_permission").is_empty(),
        "the bridge was asked"
    );
    assert!(home.work.join("build").exists(), "the command ran");
    let denied = acp
        .updates(&session)
        .into_iter()
        .find(|u| u["toolCallId"] == "call-rm" && u["sessionUpdate"] == "tool_call_update")
        .expect("the call ended");
    assert_eq!(denied["status"], "failed");
    assert!(
        !denied["content"][0]["content"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown tool"),
        "the approval, not a missing tool, stopped it: {denied}"
    );
    let listed = home.rpc("session.list", &json!({"source": "acp"}));
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["session_id"] == session)
    );
}

/// With `approvals.unattended_mode = "approve"` the same command runs,
/// still without asking the bridge.
#[test]
fn an_unattended_session_follows_approvals_unattended_mode() {
    let home = Home::new();
    home.config("\n[approvals]\nunattended_mode = \"approve\"\n");
    rm_script(&home);
    std::fs::create_dir_all(home.work.join("build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &["--unattended"], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let id = acp.send_prompt(&session, "clean");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(acp.requests("session/request_permission").is_empty());
    let results: Vec<_> = acp
        .updates(&session)
        .into_iter()
        .filter(|u| u["sessionUpdate"] == "tool_call_update")
        .collect();
    assert!(
        !home.work.join("build").exists(),
        "approved by the unattended mode: {results:?}"
    );
}

/// Scenario: Default unattended toolset; a client answer on an
/// unattended connection is ignored with a warning.
#[test]
fn an_unattended_session_has_no_terminal_or_code_execution() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &["--unattended"], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let listing = common::ok(&common::run(
        home.path(),
        &["tools", "list", "--session", &session],
    ));
    assert!(!exposed(&listing, "terminal"), "{listing}");
    assert!(!exposed(&listing, "execute_code"), "{listing}");
    assert!(exposed(&listing, "read_file"), "{listing}");
    acp.write_line(r#"{"jsonrpc":"2.0","id":4242,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}"#);
    acp.pump(Duration::from_millis(500));
    assert!(
        acp.stderr()
            .contains("warn: ignored a client answer on an unattended connection"),
        "{}",
        acp.stderr()
    );
}
