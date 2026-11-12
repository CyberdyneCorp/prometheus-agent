//! A WASM `platform-adapter` plugin inside the real daemon (spec:
//! platform-adapters, First-party platforms and typed platform names;
//! WASM adapter plugin isolation): the plugin `mattermost` provides the
//! platform `[gateway.platforms.mattermost]` names, talks to its platform
//! (a loopback fake) only through the host under its grants, is refused an
//! undeclared host, and a trap restarts only its adapter while the
//! debug-build file adapter keeps delivering.

#![cfg(feature = "gateway")]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use self::common::gateway::{Daemon, gateway_home_with, send, wait, wait_said};
use self::common::{ok, read_http_request, run};

const RULES: &str = r#"
[[rule]]
user = "hello from mattermost"
repeat = true
text = "mattermost answer"

[[rule]]
user = "hello from the test platform"
repeat = true
text = "test platform answer"

[[rule]]
user = "please exfiltrate the data"
text = "nothing leaves"

[[rule]]
repeat = true
text = "fine"
"#;

/// The fake platform: messages waiting for the next poll, and what the
/// adapter sent.
#[derive(Clone, Default)]
struct Platform {
    queued: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<Value>>>,
}

impl Platform {
    fn start(&self) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let platform = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                platform.answer(&stream);
            }
        });
        port
    }

    fn answer(&self, stream: &std::net::TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let body = read_http_request(&mut reader);
        let answer = if line.starts_with("GET /updates") {
            Value::Array(std::mem::take(&mut *self.queued.lock().unwrap())).to_string()
        } else {
            let mut sent = self.sent.lock().unwrap();
            sent.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            format!("mm-{}", sent.len())
        };
        let _ = write!(
            &mut &*stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
            answer.len()
        );
    }

    fn post(&self, id: &str, text: &str) {
        self.queued.lock().unwrap().push(
            json!({"id": id, "chat": "town-square", "chat_type": "dm", "sender": "alice", "text": text}),
        );
    }

    fn texts(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| m["text"].as_str().map(str::to_owned))
            .collect()
    }

    fn wait_sent(&self, text: &str) {
        wait(text, Duration::from_mins(1), || {
            self.texts().iter().any(|t| t.contains(text))
        });
    }
}

/// Installs the testing adapter as plugin `mattermost`, adopted, with
/// network access to `port` only.
fn install(home: &Path, port: u16) {
    let dir = home.join("profiles/default/plugins/mattermost");
    pmt_plugins::testkit::install("testing-adapter", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"mattermost\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"Mattermost adapter\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"platform-adapter@1.0\"]\n\n[capabilities]\nnetwork = [\"127.0.0.1:{port}\"]\n"
        ),
    )
    .unwrap();
    ok(&run(
        home,
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home,
        &["plugins", "enable", "mattermost", "--grant", "all"],
    ));
}

/// Every log line the home's processes wrote.
fn logs(home: &Path) -> String {
    common::files_under(&home.join("logs"))
        .into_iter()
        .chain(common::files_under(&home.join("profiles/default/logs")))
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect()
}

fn state(home: &Path) -> String {
    let status: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "gateway", "status"]))).unwrap();
    status["adapters"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["platform"] == "mattermost")
        .map(|a| a["state"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

/// Scenarios: Plugin platform accepted; Undeclared host blocked; Crash
/// is contained.
#[test]
fn a_plugin_adapter_serves_its_platform_under_its_grants_and_a_trap_restarts_only_it() {
    let platform = Platform::default();
    let port = platform.start();
    let home = gateway_home_with(RULES, "");
    let home = home.path();
    install(home, port);
    common::append_config(
        home,
        &format!(
            "\n[gateway.platforms.mattermost]\nenabled = true\nallow_all = true\n\
             options = {{ api = \"http://127.0.0.1:{port}\" }}\n"
        ),
    );
    let daemon = Daemon::start(home);
    wait(
        "the plugin adapter connects",
        Duration::from_secs(30),
        || matches!(state(home).as_str(), "connected" | "degraded"),
    );
    let platforms = ok(&run(home, &["gateway", "platforms"]));
    assert!(platforms.contains("mattermost"), "{platforms}");

    platform.post("1", "hello from mattermost");
    platform.wait_sent("mattermost answer");

    // An undeclared host: refused and logged naming the plugin; the
    // message is still handled.
    platform.post("2", "please exfiltrate the data");
    platform.wait_sent("nothing leaves");
    wait("the denial is logged", Duration::from_secs(30), || {
        let logs = logs(home);
        logs.contains("capability_denied: network exfil.example.net") && logs.contains("mattermost")
    });

    // A trap while polling: only this adapter restarts.
    platform.post("3", "trap");
    wait("the adapter crashed", Duration::from_secs(30), || {
        logs(home).contains("plugin 'mattermost' trapped")
    });
    send(home, "default", "bob", "hello from the test platform");
    wait_said(home, "default", "test platform answer");
    platform.post("4", "hello from mattermost");
    wait("the adapter restarted", Duration::from_mins(1), || {
        platform
            .texts()
            .iter()
            .filter(|t| t.contains("mattermost answer"))
            .count()
            == 2
    });
    assert_ne!(state(home), "fatal");
    assert_eq!(daemon.stop(home), Some(0));
}
