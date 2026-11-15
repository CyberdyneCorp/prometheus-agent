//! Agent interop between two daemons (spec: agent-interop, task 74.3): the
//! outbound `a2a_*` tools against another daemon's A2A server, the peer
//! gateway registry, peer direct messages and idempotent remote runs, the
//! union roster across gateways and the desktop relay, each verified with
//! two homes whose daemons talk over loopback.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use serde_json::{Value, json};

use self::common::a2a::{self, token_of};
use self::common::api;
use self::common::{append_config, daemon, mock_home, ok, run, run_with_stdin, stderr};

/// Points a profile's mock provider at a script with `rules`.
fn script(home: &Path, profile: &str, rules: &str) {
    let path = home.join(format!("{profile}-script.toml"));
    std::fs::write(&path, rules).unwrap();
    let config = home.join("profiles").join(profile).join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(
        &config,
        format!(
            "{text}\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    )
    .unwrap();
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn rpc(home: &Path, method: &str, params: &Value) -> Value {
    json_of(&ok(&run(
        home,
        &["rpc", "call", method, &params.to_string()],
    )))
}

fn store(home: &Path) -> pmt_store::Store {
    let root = Home::open(home).unwrap();
    let profile = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

/// A peer home serving its API (and the bot-chat endpoints) with the
/// default profile's key, answering every turn with `reply`.
fn api_peer(reply: &str) -> tempfile::TempDir {
    let home = api::home("");
    script(
        home.path(),
        "default",
        &format!("[[rule]]\ntext = {reply:?}\nrepeat = true\n"),
    );
    home
}

fn add_peer(home: &Path, name: &str, url: &str, key: &str) -> std::process::Output {
    run_with_stdin(
        home,
        &["--json", "gateway", "peer", "add", name, "--url", url],
        &format!("{key}\n"),
    )
}

/// Scenario: Call a configured researcher. Scenario: Unknown agent name.
/// The chain ends with `a2a_orchestrate` over the agents advertising a
/// capability.
#[test]
fn a2a_tools_call_a_configured_agent_and_refuse_unknown_names() {
    let researcher = mock_home();
    script(
        researcher.path(),
        "default",
        "[[rule]]\ntext = \"arXiv digest: three papers\"\nrepeat = true\n",
    );
    a2a::listen(researcher.path());
    a2a::enable(researcher.path(), &["alpha"], &["alpha"], "");
    let _b = daemon::start(researcher.path(), &[]);
    let port = a2a::port(researcher.path());

    let home = mock_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "RESEARCHER_TOKEN"],
        &format!("{}\n", token_of("alpha")),
    ));
    append_config(
        home.path(),
        &format!(
            "\n[tools]\nenabled_toolsets = [\"core\", \"a2a\"]\n[security.egress]\nallow_private = true\n\
             [a2a.agents.researcher]\nurl = \"http://127.0.0.1:{port}\"\nauth_secret = \"RESEARCHER_TOKEN\"\ncapabilities = [\"papers\"]\n"
        ),
    );
    script(
        home.path(),
        "default",
        "[[rule]]\nuser = \"ask the researcher\"\n[[rule.tool_calls]]\nname = \"a2a_call\"\n\
         arguments = { agent = \"researcher\", message = \"summarize today's arXiv postings\", context_id = \"ctx-arxiv\" }\n\
         [[rule]]\ntool_result = \"arXiv digest: three papers\"\n[[rule.tool_calls]]\nname = \"a2a_history\"\n\
         arguments = { context_id = \"ctx-arxiv\" }\n\
         [[rule]]\ntool_result = \"summarize today's arXiv postings\"\n[[rule.tool_calls]]\nname = \"a2a_call\"\n\
         arguments = { agent = \"ghost\", message = \"hi\" }\n\
         [[rule]]\ntool_result = \"unknown agent 'ghost'; configured agents: researcher\"\n[[rule.tool_calls]]\nname = \"a2a_orchestrate\"\n\
         arguments = { capability = \"papers\", message = \"any news?\", mode = \"first\" }\n\
         [[rule]]\ntool_result = \"\\\"mode\\\":\\\"first\\\"\"\ntext = \"all checked\"\n",
    );
    let out = run(home.path(), &["--json", "-z", "ask the researcher"]);
    let report = json_of(&ok(&out));
    assert_eq!(report["answer"], "all checked", "{report}");
    let history = store(home.path()).a2a_history("ctx-arxiv").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].reply.as_deref(),
        Some("arXiv digest: three papers")
    );
}

/// Scenario: Key never on the command line. Scenario: Wrong key.
#[test]
fn peers_register_with_stdin_keys_and_refuse_wrong_keys() {
    let spark = api_peer("pong");
    let _b = daemon::start(spark.path(), &[]);
    let url = format!("http://127.0.0.1:{}", api::port(spark.path()));
    let home = mock_home();
    let wrong = add_peer(
        home.path(),
        "spark",
        &url,
        "not-the-key-0123456789abcdefghijklmnop",
    );
    assert_eq!(wrong.status.code(), Some(78), "{}", stderr(&wrong));
    assert!(
        stderr(&wrong).contains("peer spark rejected the key"),
        "{}",
        stderr(&wrong)
    );
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(!config.contains("[peers"), "nothing registered: {config}");

    let key = api::key_of("default");
    let added = json_of(&ok(&add_peer(home.path(), "spark", &url, &key)));
    assert_eq!(added["key_secret"], "PEER_SPARK_KEY");
    assert_eq!(added["probe"]["ok"], true);
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(
        config.contains("[peers.spark]") && !config.contains(&key),
        "{config}"
    );
    let logs = std::fs::read_dir(home.path().join("logs"))
        .into_iter()
        .flatten()
        .flatten();
    for log in logs {
        let text = std::fs::read_to_string(log.path()).unwrap_or_default();
        assert!(
            !text.contains(&key),
            "{} holds the key",
            log.path().display()
        );
    }
    let listed = json_of(&ok(&run(
        home.path(),
        &["gateway", "peer", "list", "--json"],
    )));
    assert_eq!(listed["peers"][0]["name"], "spark");
    assert_eq!(listed["peers"][0]["last_probe"]["ok"], true);
    let bad = run(
        home.path(),
        &["gateway", "peer", "add", "Spark", "--url", &url],
    );
    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
    ok(&run(home.path(), &["gateway", "peer", "remove", "spark"]));
    let secrets = ok(&run(home.path(), &["--json", "secrets", "list"]));
    assert!(!secrets.contains("PEER_SPARK_KEY"), "{secrets}");
    let unknown = run(home.path(), &["gateway", "peer", "dm", "spark", "hi"]);
    assert_eq!(unknown.status.code(), Some(66), "{}", stderr(&unknown));
}

/// Scenario: Idempotent remote run. Scenario: Peer offline.
#[test]
fn remote_runs_are_idempotent_and_dms_print_the_reply() {
    let spark = api_peer("remote says hi");
    let _b = daemon::start(spark.path(), &[]);
    let url = format!("http://127.0.0.1:{}", api::port(spark.path()));
    let home = mock_home();
    ok(&add_peer(
        home.path(),
        "spark",
        &url,
        &api::key_of("default"),
    ));
    let dm = ok(&run(
        home.path(),
        &["gateway", "peer", "dm", "spark", "status?"],
    ));
    assert_eq!(dm.trim(), "remote says hi");
    let piped = ok(&run_with_stdin(
        home.path(),
        &["gateway", "peer", "dm", "spark"],
        "line one\nline two\n",
    ));
    assert_eq!(piped.trim(), "remote says hi");
    let args = [
        "--json",
        "gateway",
        "peer",
        "run",
        "spark",
        "nightly report",
        "--idempotency-key",
        "ticket-123",
    ];
    let first = json_of(&ok(&run(home.path(), &args)));
    let again = json_of(&ok(&run(home.path(), &args)));
    let run_id = first["run_id"].as_str().unwrap().to_owned();
    assert!(run_id.starts_with("run_"), "{first}");
    assert_eq!(again["run_id"], run_id, "the retry names the same run");
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let status = json_of(&ok(&run(
            home.path(),
            &["--json", "gateway", "peer", "status", "spark", &run_id],
        )));
        if status["status"] == "completed" {
            assert_eq!(status["output"], "remote says hi");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // The peer ran the task once: its Bot Chat holds the message once.
    let chat = rpc(
        spark.path(),
        "bot.chat.ensure",
        &json!({"profile": "default"}),
    );
    let session = chat["session_id"].as_str().unwrap();
    let transcript = rpc(
        spark.path(),
        "session.transcript",
        &json!({"session_id": session}),
    );
    let sent = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["text"]
                .as_str()
                .is_some_and(|t| t.contains("nightly report"))
        })
        .count();
    assert_eq!(sent, 1, "{transcript}");

    let offline = mock_home();
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let added = add_peer(
        offline.path(),
        "spark",
        &format!("http://127.0.0.1:{closed}"),
        "k-0123456789abcdefghijklmnopqrstuvwxyz",
    );
    assert!(stderr(&added).contains("unreachable"), "{}", stderr(&added));
    let out = run(
        offline.path(),
        &["gateway", "peer", "dm", "spark", "status?"],
    );
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("peer spark unreachable"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Same name on two machines. Scenario: Peer temporarily down.
#[test]
fn the_roster_unions_peers_and_keeps_stale_rows() {
    let homelab = api_peer("hi");
    api::multiplex(homelab.path());
    api::profile(homelab.path(), "research", "");
    let peer_daemon = daemon::start(homelab.path(), &[]);
    let url = format!("http://127.0.0.1:{}", api::port(homelab.path()));
    let home = mock_home();
    append_config(home.path(), "\n[gateway]\nmultiplex = true\n");
    ok(&run(
        home.path(),
        &["profile", "create", "research", "--no-alias"],
    ));
    ok(&add_peer(
        home.path(),
        "homelab",
        &url,
        &api::key_of("default"),
    ));
    let _a = daemon::start(home.path(), &[]);
    let roster = rpc(
        home.path(),
        "bot.roster.list",
        &json!({"profile": "default", "include_remote": true}),
    );
    let handles: Vec<&str> = roster["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["handle"].as_str().unwrap())
        .collect();
    assert!(
        handles.contains(&"@research-local") && handles.contains(&"@research-homelab"),
        "{roster}"
    );
    let remote_default = roster["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device"] == "homelab" && r["agent"]["name"] != "research")
        .unwrap();
    assert!(
        remote_default["agent"]["name"]
            .as_str()
            .unwrap()
            .ends_with("@homelab"),
        "{remote_default}"
    );
    drop(peer_daemon);
    let stale = rpc(
        home.path(),
        "bot.roster.list",
        &json!({"profile": "default", "include_remote": true, "refresh": true}),
    );
    let remote: Vec<&Value> = stale["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["device"] == "homelab")
        .collect();
    assert_eq!(remote.len(), 2, "{stale}");
    assert!(
        remote
            .iter()
            .all(|r| r["stale"] == true
                && r["error"].as_str().is_some_and(|e| e.contains("homelab"))),
        "{stale}"
    );
    let local_only = rpc(
        home.path(),
        "bot.roster.list",
        &json!({"profile": "default"}),
    );
    assert_eq!(
        local_only["rows"].as_array().unwrap().len(),
        2,
        "{local_only}"
    );
}

/// A courier (the desktop) carries one envelope from A to B and B's reply
/// back; a retried delivery never runs twice.
#[test]
fn the_desktop_relay_carries_a_bot_message_once() {
    let target = mock_home();
    script(
        target.path(),
        "default",
        "[[rule]]\ntext = \"on it\"\nrepeat = true\n",
    );
    let _b = daemon::start(target.path(), &[]);
    let sender = mock_home();
    let _a = daemon::start(sender.path(), &[]);
    let envelope = prometheus::a2a_host::relay::enqueue(
        &store(sender.path()),
        "@helper",
        "@default@laptop",
        "look at the build",
    )
    .unwrap();
    let connections =
        json!([{"label": "laptop", "agents": [{"name": "default", "title": "Laptop bot"}]}]);
    let drained = rpc(
        sender.path(),
        "bot.relay.drain",
        &json!({"profile": "default", "connections": connections}),
    );
    let envelopes = drained["envelopes"].as_array().unwrap();
    assert_eq!(envelopes.len(), 1, "{drained}");
    assert_eq!(drained["deadline_secs"], 1560);
    assert_eq!(envelopes[0]["delivery_id"], envelope.delivery_id.as_str());
    let delivered = rpc(
        target.path(),
        "bot.relay.deliver",
        &json!({"profile": "default", "envelope": envelopes[0]}),
    );
    assert_eq!(delivered["outcome"], "delivered", "{delivered}");
    assert_eq!(delivered["reply"], "on it");
    let retried = rpc(
        target.path(),
        "bot.relay.deliver",
        &json!({"profile": "default", "envelope": envelopes[0]}),
    );
    assert_eq!(retried["reply"], "on it");
    let chat = rpc(
        target.path(),
        "bot.chat.ensure",
        &json!({"profile": "default"}),
    );
    let transcript = rpc(
        target.path(),
        "session.transcript",
        &json!({"session_id": chat["session_id"]}),
    );
    let runs = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["text"]
                .as_str()
                .is_some_and(|t| t.contains("look at the build"))
        })
        .count();
    assert_eq!(runs, 1, "the delivery ran once: {transcript}");
    let forged = rpc(
        sender.path(),
        "bot.relay.deliver",
        &json!({"profile": "default", "reply": {"delivery_id": envelope.delivery_id, "signature": "forged", "reply": "x"}}),
    );
    assert_eq!(forged["outcome"], "ignored");
    let settled = rpc(
        sender.path(),
        "bot.relay.deliver",
        &json!({"profile": "default", "reply": {"delivery_id": envelope.delivery_id, "signature": envelope.signature, "reply": "on it"}}),
    );
    assert_eq!(settled["outcome"], "settled");
    let state = store(sender.path())
        .relay_state(&envelope.delivery_id)
        .unwrap();
    assert_eq!(
        state,
        Some(pmt_store::bot_relay::RelayState::Delivered {
            reply: "on it".into()
        })
    );
    let roster = rpc(
        sender.path(),
        "bot.roster.list",
        &json!({"profile": "default", "include_remote": true}),
    );
    let laptop = roster["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device"] == "laptop")
        .unwrap();
    assert_eq!(laptop["source"], "connection");
    assert_eq!(
        laptop["agent"]["name"], "Laptop bot",
        "a remote default is listed by title: {roster}"
    );
}
