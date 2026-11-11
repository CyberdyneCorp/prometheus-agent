//! Observability reports on temporary homes (spec: observability, Crash
//! and debug bundle; Usage insights report; Configuration-centric status;
//! Export posture status; Opt-in debug share upload; `ShareGPT` trajectory
//! export).

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{append_config, command, mock_home, ok, run, run_with_stdin, stderr, stdout};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{NewMessage, NewSession, Origin, Role, Source, Store, Tokens};
use serde_json::Value;

/// A local HTTP endpoint: every request (method, path, body) is kept and
/// answered with `answer`.
/// Requests an [`endpoint`] received: (method and path, body).
type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn endpoint(answer: &'static str) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let keep = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            answer_one(stream, &keep, answer);
        }
    });
    (url, seen)
}

/// Keeps one request of `stream` and answers it with `answer`.
fn answer_one(stream: std::net::TcpStream, keep: &Seen, answer: &str) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let body = common::read_http_request(&mut reader);
    let request = line
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    keep.lock()
        .unwrap()
        .push((request, String::from_utf8_lossy(&body).into_owned()));
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
        answer.len()
    );
    let _ = writer.write_all(response.as_bytes());
}

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

/// Scenario: Bundle contains no secret values.
#[test]
fn a_debug_bundle_contains_none_of_the_profiles_secret_values() {
    let home = mock_home();
    let secrets = [
        ("OPENAI_API_KEY", "sk-bundle-test-0123456789abcdef"),
        ("GITHUB_TOKEN", "plain-secret-value-two-xyz"),
        ("OTLP_API_KEY", "third-secret-value-qwerty"),
    ];
    for (name, value) in secrets {
        ok(&run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            value,
        ));
    }
    // Values that reached files before redaction could catch them.
    let crash = home.path().join("logs/crash");
    std::fs::create_dir_all(&crash).unwrap();
    std::fs::write(
        crash.join("crash-1.txt"),
        format!("panic with {}\n", secrets[0].1),
    )
    .unwrap();
    let logs = home.path().join("profiles/default/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let record = serde_json::json!({"ts": jiff::Timestamp::now().to_string(), "level": "warn", "msg": format!("token {}", secrets[1].1)});
    std::fs::write(logs.join("prometheus.log"), format!("{record}\n")).unwrap();
    let id = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W";
    let trajectories = home.path().join("profiles/default/trajectories");
    std::fs::create_dir_all(&trajectories).unwrap();
    std::fs::write(
        trajectories.join(format!("{id}.jsonl")),
        format!(
            "{{\"type\":\"user.message\",\"payload\":{{\"text\":\"{}\"}}}}\n",
            secrets[2].1
        ),
    )
    .unwrap();
    let bundle = home.path().join("bundle.tar.zst");
    let out = run(
        home.path(),
        &[
            "debug",
            "bundle",
            "--session",
            id,
            "-o",
            bundle.to_str().unwrap(),
        ],
    );
    assert!(
        stdout(&out).contains("debug bundle written to"),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let mut archive =
        tar::Archive::new(zstd::Decoder::new(std::fs::File::open(&bundle).unwrap()).unwrap());
    let mut names = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().display().to_string();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        for (secret, value) in secrets {
            assert!(!text.contains(value), "{name} holds the value of {secret}");
        }
        names.push(name);
    }
    for expected in [
        "doctor.json",
        "version.json",
        "config/config.toml",
        "crash/crash-1.txt",
        "logs/profile/prometheus.log",
        "manifest.json",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} missing from {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == &format!("trajectory/{id}.jsonl")),
        "{names:?}"
    );
}

fn gateway_session(store: &Store, platform: &str) -> pmt_core::id::SessionId {
    let mut new = NewSession::new(Source::Gateway);
    new.origin = Origin::Gateway {
        platform: platform.to_owned(),
        chat: "c1".to_owned(),
        thread: None,
        user: None,
    };
    store.create_session(&new).unwrap().id
}

/// Scenario: Insights for one platform.
#[test]
fn insights_for_one_platform_count_only_its_sessions_and_calls() {
    let home = mock_home();
    let scope = scope(home.path());
    let store = Arc::new(Store::open(&scope).unwrap());
    let recorder = pmt_obs::usage::UsageRecorder::new(
        Arc::clone(&store),
        Arc::new(pmt_obs::usage::ConfigPrices::default()),
    );
    let record = |session, provider: &str| {
        let mut row = pmt_obs::usage::Call::new(session, "main", provider, "m");
        row.turn_id = Some(format!("turn_{}", ulid::Ulid::generate()));
        row.tokens = Tokens {
            input: 10,
            output: 5,
            ..Tokens::default()
        };
        recorder.record(row).unwrap();
    };
    for _ in 0..12 {
        let id = gateway_session(&store, "telegram");
        store
            .append_message(id, &NewMessage::new(Role::User, "hi"))
            .unwrap();
        record(id, "tg-provider");
    }
    for _ in 0..30 {
        let id = store
            .create_session(&NewSession::new(Source::Cli))
            .unwrap()
            .id;
        store
            .append_message(id, &NewMessage::new(Role::User, "hello"))
            .unwrap();
        record(id, "cli-provider");
    }
    drop(store);
    let out = run(
        home.path(),
        &["--json", "usage", "insights", "--source", "telegram"],
    );
    let body: Value = serde_json::from_str(&ok(&out)).unwrap();
    let report = &body["profiles"][0];
    assert_eq!(report["overview"]["sessions"], 12, "{report}");
    assert_eq!(report["overview"]["user_messages"], 12);
    assert_eq!(report["overview"]["turns"], 12);
    let providers: Vec<&str> = report["by_provider"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(providers, ["tg-provider"]);
    let sources: Vec<&str> = report["by_source"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["telegram"]);
    // Every source together.
    let all: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "usage", "insights"]))).unwrap();
    assert_eq!(all["profiles"][0]["overview"]["sessions"], 42);
    // An unknown source exits 2 naming the sources present.
    let out = run(home.path(), &["usage", "insights", "--source", "discord"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("cli, telegram"), "{}", stderr(&out));
}

/// Scenario: Offline full status.
#[test]
fn full_status_lists_credentials_offline_and_only_deep_probes_connect() {
    let home = mock_home();
    let (url, seen) = endpoint("{}");
    append_config(
        home.path(),
        &format!("\n[providers.acme]\nbase_url = \"{url}/v1\"\nkey_secret = \"ACME_API_KEY\"\n"),
    );
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "acme"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "gpt-x"],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "ACME_API_KEY"],
        "sk-status-test-0123456789",
    ));
    let out = run(home.path(), &["status", "--full"]);
    let text = ok(&out);
    assert!(text.contains("providers with credentials: acme"), "{text}");
    assert!(text.contains("model: gpt-x / provider acme"), "{text}");
    assert!(
        text.contains("sessions: 0 active, 0 today, 0 total"),
        "{text}"
    );
    assert!(!text.contains("sk-status-test"), "{text}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        seen.lock().unwrap().is_empty(),
        "status --full connected to {:?}",
        seen.lock().unwrap()
    );
    let out = run(home.path(), &["status", "--full", "--deep"]);
    let text = ok(&out);
    assert!(text.contains("provider acme: ok in"), "{text}");
    assert_eq!(
        seen.lock().unwrap().first().map(|(r, _)| r.as_str()),
        Some("GET /v1/models")
    );
}

/// Scenario: Deep status probes every adapter endpoint.
#[cfg(feature = "gateway")]
#[test]
fn deep_status_probes_every_configured_adapter_endpoint() {
    let home = mock_home();
    let (homeserver, seen) = endpoint("{}");
    let imap = TcpListener::bind("127.0.0.1:0").unwrap();
    let smtp = TcpListener::bind("127.0.0.1:0").unwrap();
    // A port nothing listens on: the probe must name it and say why.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    append_config(
        home.path(),
        &format!(
            "\n[gateway.platforms.matrix]\nenabled = true\noptions = {{ homeserver = \"{homeserver}\" }}\n\
             \n[gateway.platforms.email]\nenabled = true\noptions = {{ address = \"bot@example.org\", \
             imap_host = \"127.0.0.1\", imap_port = {}, smtp_host = \"127.0.0.1\", smtp_port = {}, \
             require_authenticated_sender = false }}\n\
             \n[gateway.platforms.signal]\nenabled = true\noptions = {{ http_url = \"http://127.0.0.1:{closed}\", \
             account = \"+15550100\" }}\n\
             \n[gateway.platforms.webhook]\nenabled = true\n",
            imap.local_addr().unwrap().port(),
            smtp.local_addr().unwrap().port(),
        ),
    );
    let text = ok(&run(home.path(), &["status", "--full", "--deep"]));
    assert!(text.contains("adapter matrix: ok in"), "{text}");
    assert!(text.contains("adapter email: ok in"), "{text}");
    assert!(
        text.contains(&format!(
            "adapter signal: failed: http://127.0.0.1:{closed}: "
        )),
        "{text}"
    );
    assert!(text.contains("adapter webhook: inbound only"), "{text}");
    assert_eq!(
        seen.lock().unwrap().first().map(|(r, _)| r.as_str()),
        Some("GET /")
    );
    let json: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "status", "--full", "--deep"],
    )))
    .unwrap();
    let webhook = json["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "webhook")
        .unwrap();
    assert_eq!(webhook["inbound_only"], true, "{json}");
}

/// Scenario: Content capture is visible.
#[test]
fn export_posture_states_what_leaves_the_host() {
    let home = mock_home();
    let out = run(home.path(), &["status", "--exports"]);
    assert_eq!(ok(&out), "no telemetry leaves this host\n");
    append_config(
        home.path(),
        "\n[observability.otel]\nenabled = true\nendpoint = \"https://collector.example:4318\"\ncapture_content = true\n",
    );
    let text = ok(&run(home.path(), &["status", "--exports"]));
    assert!(
        text.contains("otlp_traces      enabled -> collector.example:4318"),
        "{text}"
    );
    assert!(
        text.contains("message content and tool arguments are exported to collector.example:4318"),
        "{text}"
    );
    let json: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "status", "--exports"]))).unwrap();
    assert_eq!(json["capture_content"], true);
    assert_eq!(json["nothing_leaves"], false);
}

/// Scenario: Unattended share refused.
#[test]
fn an_unattended_share_without_yes_uploads_nothing_and_exits_4() {
    let home = mock_home();
    let (url, seen) = endpoint(r#"{"url":"http://share.test/r/1"}"#);
    append_config(
        home.path(),
        &format!("\n[debug.share]\nendpoint = \"{url}/reports\"\n"),
    );
    let out = run(home.path(), &["debug", "share", "--target", "custom"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing was uploaded"),
        "{}",
        stderr(&out)
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(seen.lock().unwrap().is_empty());
    // With --yes the same report goes to the endpoint and is recorded.
    let text = ok(&run(
        home.path(),
        &["debug", "share", "--target", "custom", "--yes"],
    ));
    assert!(text.contains("uploaded: http://share.test/r/1"), "{text}");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests[0].0, "POST /reports");
    assert!(requests[0].1.contains("version"), "{}", requests[0].1);
    let shares = std::fs::read_to_string(home.path().join("logs/shares.json")).unwrap();
    assert!(shares.contains("http://share.test/r/1"), "{shares}");
}

/// Scenario: Declined upload sends nothing.
#[cfg(unix)]
#[test]
fn a_declined_upload_makes_no_connection_and_exits_1() {
    use expectrl::{Eof, Expect};
    let home = mock_home();
    let (url, seen) = endpoint("{}");
    append_config(
        home.path(),
        &format!("\n[debug.share]\nendpoint = \"{url}/reports\"\n"),
    );
    let mut p = common::pty::spawn(home.path(), &["debug", "share", "--target", "custom"]);
    p.expect("It will be public to anyone with the link. [y/N]")
        .unwrap();
    p.send_line("n").unwrap();
    p.expect("nothing was uploaded").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 1)"), "{status:?}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
}

#[test]
fn a_local_share_writes_the_report_without_network() {
    let home = mock_home();
    let text = ok(&run(home.path(), &["debug", "share", "--local"]));
    let path = text
        .trim()
        .strip_prefix("report written to ")
        .unwrap()
        .split(" (")
        .next()
        .unwrap();
    let report = std::fs::read_to_string(path).unwrap();
    assert!(report.contains("version"), "{report}");
}

/// Scenario: Tool call rendering (end to end).
#[test]
fn an_exported_session_renders_its_tool_call_and_response() {
    let home = mock_home();
    std::fs::write(home.path().join("notes.txt"), "buy milk\n").unwrap();
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "read the notes"
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "notes.txt" }

[[rule]]
tool_result_regex = "(?s).*"
text = "they say buy milk"
"#,
    )
    .unwrap();
    let out = command(home.path())
        .current_dir(home.path())
        .args(["-z", "read the notes", "--mock-script"])
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let file = home.path().join("export.jsonl");
    let text = ok(&run(
        home.path(),
        &[
            "trajectory",
            "export",
            "--format",
            "sharegpt",
            "-o",
            file.to_str().unwrap(),
        ],
    ));
    assert!(text.contains("exported 1 session(s)"), "{text}");
    let line: Value = serde_json::from_str(
        std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let turns = line["conversations"].as_array().unwrap();
    let call = turns
        .iter()
        .position(|t| {
            t["from"] == "gpt"
                && t["value"]
                    .as_str()
                    .unwrap()
                    .contains("<tool_call>{\"name\":\"read_file\"")
        })
        .unwrap();
    assert_eq!(turns[call + 1]["from"], "tool");
    assert!(
        turns[call + 1]["value"]
            .as_str()
            .unwrap()
            .starts_with("<tool_response>{\"tool_call_id\""),
        "{line}"
    );
    assert_eq!(turns[0]["from"], "system", "{line}");
    assert!(
        line["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "read_file"),
        "{line}"
    );
    assert_eq!(line["meta"]["completed"], true);
}
