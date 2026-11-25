//! Task 81.3, A2A: an agent calls a peer daemon's A2A server with
//! `a2a_call`; the peer runs the request as a turn of its own A2A session
//! and the completed task's answer comes back into the caller's turn
//! (spec: agent-interop, A2A server; Outbound A2A tools).

use std::path::Path;

use serde_json::{Value, json};

use crate::common::a2a::{self, token_of};
use crate::common::{append_config, daemon, mock_home, ok, run, run_with_stdin};

fn script(home: &Path, rules: &str) {
    let path = home.join("a2a-script.toml");
    std::fs::write(&path, rules).unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    );
}

#[test]
fn an_a2a_peer_completes_a_remote_run() {
    let peer = mock_home();
    script(
        peer.path(),
        "[[rule]]\nuser_regex = 'quarterly numbers'\ntext = \"revenue up 12%\"\n",
    );
    a2a::listen(peer.path());
    a2a::enable(peer.path(), &["hq"], &["hq"], "");
    let _peer = daemon::start(peer.path(), &[]);
    let port = a2a::port(peer.path());

    let home = mock_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "ANALYST_TOKEN"],
        &format!("{}\n", token_of("hq")),
    ));
    append_config(
        home.path(),
        &format!(
            "\n[tools]\nenabled_toolsets = [\"core\", \"a2a\"]\n[security.egress]\nallow_private = true\n\
             [a2a.agents.analyst]\nurl = \"http://127.0.0.1:{port}\"\nauth_secret = \"ANALYST_TOKEN\"\n"
        ),
    );
    script(
        home.path(),
        "[[rule]]\nuser = \"ask the analyst\"\n[[rule.tool_calls]]\nname = \"a2a_call\"\n\
         arguments = { agent = \"analyst\", message = \"what are the quarterly numbers?\", context_id = \"ctx-q3\" }\n\
         [[rule]]\ntool_result_regex = 'revenue up 12%'\ntext = \"the analyst says revenue is up 12%\"\n",
    );
    let out = ok(&run(home.path(), &["--json", "-z", "ask the analyst"]));
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        report["answer"], "the analyst says revenue is up 12%",
        "{report}"
    );

    // The peer ran it as its own A2A turn, and the task is completed.
    let listed: Value = serde_json::from_str(&ok(&run(
        peer.path(),
        &["--json", "sessions", "list", "--all"],
    )))
    .unwrap();
    let remote: Vec<&Value> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["title"].as_str().is_some_and(|t| t.starts_with("A2A ")))
        .collect();
    assert_eq!(remote.len(), 1, "{listed}");
    let tasks = a2a::rpc(
        port,
        Some("hq"),
        "ListTasks",
        &json!({"contextId": "ctx-q3"}),
    );
    let tasks = tasks.json();
    let task = &tasks["result"]["tasks"][0];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{tasks}");
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], "revenue up 12%");
}
