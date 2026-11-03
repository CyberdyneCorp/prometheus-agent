//! `/handoff` from the line interface to the real daemon's gateway (spec:
//! messaging-gateway, Session handoff to a messaging platform).

#![cfg(all(unix, feature = "gateway"))]

mod common;

use std::process::Command;
use std::time::Duration;

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

use self::common::gateway::{Daemon, gateway_home_with, send_in, sent, wait, wait_said};
use self::common::{ok, run};

const RULES: &str = "[[rule]]\nrepeat = true\ntext = \"fast done\"\n";

fn chat(home: &std::path::Path) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args(["--set", "ui.interface=line", "--no-color"])
        .current_dir(home)
        .env("TERM", "xterm");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_mins(1)));
    session
}

/// Spec scenario: Handoff to Telegram home (on the test platform).
#[test]
fn a_cli_session_continues_in_a_new_thread_of_the_home_channel() {
    let home = gateway_home_with(RULES, "home_channel = { chat_id = \"u1\" }\n");
    let home = home.path();
    let daemon = Daemon::start(home);
    let mut p = chat(home);
    p.expect("› ").unwrap();
    p.send_line("plan the trip").unwrap();
    p.expect("fast done").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/handoff test").unwrap();
    let found = p
        .expect(expectrl::Regex(
            "resume locally with prometheus chat --resume (ses_[0-9A-Z]+)",
        ))
        .unwrap();
    let session = String::from_utf8_lossy(&found[1]).into_owned();
    p.expect("continued on test").ok();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");

    wait("the thread is opened", Duration::from_secs(30), || {
        sent(home, "default").iter().any(|s| s["kind"] == "thread")
    });
    let thread = sent(home, "default")
        .into_iter()
        .find(|s| s["kind"] == "thread")
        .unwrap();
    assert_eq!(thread["chat"], "u1");
    let thread = thread["thread"].as_str().unwrap().to_owned();
    wait_said(home, "default", "You: plan the trip");
    let replay = sent(home, "default")
        .into_iter()
        .find(|s| {
            s["text"]
                .as_str()
                .is_some_and(|t| t.contains("You: plan the trip"))
        })
        .unwrap();
    assert_eq!(replay["thread"], thread.as_str());
    assert!(
        replay["text"]
            .as_str()
            .unwrap()
            .contains("Assistant: fast done")
    );

    send_in(home, "default", "u1", "u1", &thread, "and the hotel?");
    wait(
        "the thread's turn ran in the handed-off session",
        Duration::from_mins(1),
        || ok(&run(home, &["sessions", "export", &session])).contains("and the hotel?"),
    );
    let exported = ok(&run(home, &["sessions", "export", &session]));
    assert!(
        exported.contains("continues on test"),
        "the notice turn: {exported}"
    );
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
    daemon.stop(home);
}

#[test]
fn a_chat_the_profile_cannot_reach_is_refused() {
    let home = gateway_home_with(RULES, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    let mut p = chat(home);
    p.expect("› ").unwrap();
    p.send_line("/handoff test:stranger").unwrap();
    p.expect("not an authorized chat").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    daemon.stop(home);
}
