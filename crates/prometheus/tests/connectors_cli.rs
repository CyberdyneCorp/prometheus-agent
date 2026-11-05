//! Hosted connectors end to end against the fake Nous connector gateway
//! (spec: connectors, Connector availability; Remote connector tools
//! through the search bridge; Connector calls run through the dispatch
//! pipeline; Connection flows require the user; Connector account
//! management CLI).

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use pmt_tools::connectors::fake::{Fake, tool};
use serde_json::Value;

use self::common::{
    command, mock_home, newest_session, ok, run, run_with_stdin, session_messages, stderr,
};

/// A fake gateway on its own runtime, alive as long as the value.
struct Gateway {
    fake: Fake,
    runtime: tokio::runtime::Runtime,
}

impl Gateway {
    fn start(connectors: &[&str]) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let tools = vec![
            tool("gmail", "GMAIL_SEND_EMAIL", "Send an email from Gmail."),
            tool("notion", "NOTION_CREATE_PAGE", "Create a Notion page."),
        ];
        let fake = runtime.block_on(Fake::start(connectors, tools));
        Self { fake, runtime }
    }

    fn requests(&self) -> usize {
        self.runtime.block_on(self.fake.request_count())
    }
}

/// A mock-provider home pointed at `gateway`.
fn home_for(gateway: &Gateway) -> tempfile::TempDir {
    let home = mock_home();
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "connectors.gateway_url",
            &gateway.fake.url(),
        ],
    ));
    home
}

/// Signs the profile in to Nous (a stored sign-in, as `auth login nous`
/// leaves it).
fn sign_in(home: &Path) {
    let meta = r#"{"provider":"nous","label":"default","source":"device","obtained_at":"2026-01-01T00:00:00Z","status":"ok"}"#;
    for (name, value) in [
        ("OAUTH_NOUS_DEFAULT_ACCESS", "nous-token"),
        ("OAUTH_NOUS_DEFAULT_META", meta),
    ] {
        ok(&run_with_stdin(
            home,
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
}

/// An MCP server whose tools sit behind the tool-search bridge, so
/// `tool_search` exists without connectors.
fn add_local_server(home: &Path) {
    let fixture = pmt_mcp::testkit::fixture().to_string_lossy().into_owned();
    ok(&run(
        home,
        &["mcp", "add", "fx", "--command", &fixture, "--trust", "full"],
    ));
}

fn script(home: &Path, body: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

fn one_shot(home: &Path, script: &str, extra: &[&str]) -> String {
    let out = command(home)
        .args(extra)
        .args(["-z", "go", "--mock-script", script])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every tool message of the newest session.
fn tool_results(home: &Path) -> Vec<String> {
    session_messages(home, &newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m.to_string())
        .collect()
}

const SEARCH: &str = r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "tool_search"
arguments = { queries = ["send an email", "echo"] }

[[rule]]
tool_result = "mcp__fx__"
text = "searched"
"#;

#[test]
fn a_disabled_feature_is_invisible() {
    let gateway = Gateway::start(&["gmail"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    ok(&run(
        home.path(),
        &["config", "set", "connectors.enabled", "false"],
    ));
    add_local_server(home.path());
    let script = script(home.path(), SEARCH);
    let searched = one_shot(
        home.path(),
        &script,
        &["--set", "tools.tool_search.mode=on"],
    );
    assert_eq!(searched, "searched\n");
    assert_eq!(gateway.requests(), 0);
    let results = tool_results(home.path()).join("\n");
    assert!(!results.contains("connector"), "{results}");
}

#[test]
fn no_identity_is_created_and_nothing_is_sent_without_a_sign_in() {
    let gateway = Gateway::start(&["gmail"]);
    let home = home_for(&gateway);
    add_local_server(home.path());
    let script = script(home.path(), SEARCH);
    one_shot(
        home.path(),
        &script,
        &["--set", "tools.tool_search.mode=on"],
    );
    assert_eq!(gateway.requests(), 0);
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(!secrets.contains("NOUS"), "{secrets}");
}

#[test]
fn a_signed_in_session_reaches_connectors_only_through_the_bridge() {
    let gateway = Gateway::start(&["gmail"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    let script = script(
        home.path(),
        r#"
[[rule]]
tools = "\"name\":\"connectors__"
text = "a connector tool was in the array"

[[rule]]
user = "go"
tools = "\"name\":\"tool_search\""
[[rule.tool_calls]]
name = "tool_search"
arguments = { queries = ["send an email"] }

[[rule]]
tool_result = "connectors__gmail__send_email"
[[rule.tool_calls]]
name = "tool_call"
arguments = { calls = [{ name = "connectors__gmail__send_email", arguments = { to = "a@b.c" } }] }

[[rule]]
tool_result = "CONNECTION_REQUIRED"
text = "needs an account"
"#,
    );
    assert_eq!(one_shot(home.path(), &script, &[]), "needs an account\n");
    let results = tool_results(home.path()).join("\n");
    assert!(results.contains("manage_connections"), "{results}");
}

#[test]
fn a_run_without_a_card_gets_a_connect_link() {
    let gateway = Gateway::start(&["notion"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "manage_connections"
arguments = { action = "connect", connectors = ["notion"] }

[[rule]]
tool_result = "connect_url"
text = "link given"
"#,
    );
    assert_eq!(one_shot(home.path(), &script, &[]), "link given\n");
    let results = tool_results(home.path()).join("\n");
    assert!(
        results.contains("https://connect.example/notion/"),
        "{results}"
    );
}

#[test]
fn commands_need_a_sign_in_and_a_valid_alias() {
    let gateway = Gateway::start(&["gmail"]);
    let home = home_for(&gateway);
    let out = run(home.path(), &["connectors", "list"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("connectors need a Nous sign-in: run prometheus auth login nous")
    );
    let out = run(
        home.path(),
        &["connectors", "connect", "gmail", "--alias", "Work"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(gateway.requests(), 0);
}

#[test]
fn list_accounts_rename_and_connect() {
    let gateway = Gateway::start(&["gmail", "notion"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    gateway
        .fake
        .add_account("gmail", None, "me@home.org", "active");
    let old = gateway
        .fake
        .add_account("gmail", Some("work"), "me@work.org", "active");
    let listed: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "connectors", "list"]))).unwrap();
    assert_eq!(listed["connectors"][0]["connector"], "gmail");
    assert_eq!(listed["connectors"][0]["connected"], true);
    assert_eq!(listed["connectors"][1]["connected"], false);
    ok(&run(
        home.path(),
        &[
            "connectors",
            "rename",
            "gmail",
            "me@home.org",
            "--to",
            "home",
        ],
    ));
    let accounts = ok(&run(home.path(), &["connectors", "accounts", "gmail"]));
    assert!(
        accounts.contains("gmail  home  me@home.org  active"),
        "{accounts}"
    );
    // A forced reconnect replaces the account and keeps the old one retired.
    gateway.fake.complete_on_read();
    let out = ok(&run(
        home.path(),
        &[
            "connectors",
            "connect",
            "gmail",
            "--alias",
            "work",
            "--reconnect",
            "--force",
        ],
    ));
    assert!(out.contains("https://connect.example/gmail/"), "{out}");
    assert!(out.contains("gmail (account 'work'): connected"), "{out}");
    let shown = ok(&run(home.path(), &["connectors", "accounts", "gmail"]));
    assert!(!shown.contains("retired"), "{shown}");
    let all: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "connectors", "accounts", "gmail", "--all"],
    )))
    .unwrap();
    let retired: Vec<&Value> = all["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["status"] == "retired")
        .collect();
    assert_eq!(retired.len(), 1, "{all}");
    assert!(
        gateway
            .fake
            .accounts()
            .iter()
            .any(|a| a.connection_id == old && a.disabled)
    );
    gateway.fake.without_rename();
    let refused = run(
        home.path(),
        &["connectors", "rename", "gmail", "home", "--to", "personal"],
    );
    assert_eq!(refused.status.code(), Some(69));
    assert!(stderr(&refused).contains("this Nous portal cannot rename connector accounts yet"));
}

#[test]
fn connect_times_out_when_the_account_never_arrives() {
    let gateway = Gateway::start(&["notion"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    let out = run(
        home.path(),
        &[
            "--set",
            "connectors.connect_timeout_secs=1",
            "connectors",
            "connect",
            "notion",
        ],
    );
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("https://connect.example/notion/"));
}

#[test]
fn disconnect_needs_confirmation() {
    let gateway = Gateway::start(&["gmail"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    gateway
        .fake
        .add_account("gmail", Some("work"), "me@work.org", "active");
    let out = run(
        home.path(),
        &["connectors", "disconnect", "gmail", "--alias", "work"],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(gateway.fake.accounts().len(), 1, "nothing removed");
    ok(&run(
        home.path(),
        &[
            "connectors",
            "disconnect",
            "gmail",
            "--alias",
            "work",
            "--yes",
        ],
    ));
    assert!(gateway.fake.accounts().is_empty());
}

/// Ctrl-C while `connect` waits stops waiting with exit 130.
#[cfg(unix)]
#[test]
fn ctrl_c_while_waiting_exits_130() {
    use std::io::{BufRead as _, BufReader};

    let gateway = Gateway::start(&["notion"]);
    let home = home_for(&gateway);
    sign_in(home.path());
    let mut child = command(home.path())
        .args(["connectors", "connect", "notion"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let waiting = lines
        .by_ref()
        .map_while(Result::ok)
        .find(|l| l.starts_with("Waiting"));
    assert!(waiting.is_some());
    std::thread::sleep(Duration::from_millis(200));
    let killed = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
}
