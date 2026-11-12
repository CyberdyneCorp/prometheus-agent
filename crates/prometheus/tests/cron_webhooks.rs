//! Webhook-triggered scheduled job runs inside the real daemon (spec:
//! scheduled-jobs, Webhook-triggered job runs): a signed event fires the
//! route's job with the rendered event as its run context, and a second
//! event while that run is active is dropped.

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{Daemon, gateway_home_with, wait};
use self::common::{ok, run, run_with_stdin};

const SECRET: &str = "pr-signing-key";

/// One signed GitHub-style POST of `event`; the status code and body.
fn post(port: u16, event: &str, delivery: &str, body: &str) -> (u16, Value) {
    let sig = pmt_adapters_webhook::github_signature(SECRET, body.as_bytes());
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let request = format!(
        "POST /p/default/webhook/pr-feedback HTTP/1.1\r\nhost: localhost\r\n\
         x-github-event: {event}\r\nx-github-delivery: {delivery}\r\n\
         x-hub-signature-256: {sig}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    (status, body)
}

fn runs(home: &Path, job: &str) -> Vec<Value> {
    let out = ok(&run(home, &["--json", "cron", "runs", job]));
    let doc: Value = serde_json::from_str(&out).unwrap();
    doc["runs"].as_array().cloned().unwrap_or_default()
}

fn review(pr: u32) -> String {
    format!(
        r#"{{"action": "submitted", "review": {{"state": "changes_requested"}}, "pull_request": {{"number": {pr}}}}}"#
    )
}

/// Scenarios: PR review feedback fires a sweeper job; Burst does not
/// double-fire.
#[test]
fn a_signed_review_event_fires_the_sweeper_job_once() {
    let home = gateway_home_with(
        "[[rule]]\nuser_regex = '(?s)## Run Context.*changes_requested on pull request 42'\n\
         latency_ms = 3000\ntext = \"swept PR 42\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"no context\"\n",
        "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\n\n\
         [gateway.platforms.webhook]\nenabled = true\n\
         routes.pr-feedback = { secret_secret = \"PR_SECRET\", events = [\"pull_request_review\"], \
         cron_job = \"pr-review-sweeper\", \
         prompt = \"Review {review.state} on pull request {pull_request.number}\" }\n",
    );
    let home = home.path();
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.http]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    ok(&run_with_stdin(
        home,
        &["secrets", "set", "PR_SECRET"],
        SECRET,
    ));
    let created = ok(&run(
        home,
        &[
            "--json",
            "cron",
            "add",
            "every 1d",
            "--prompt",
            "Sweep the open review feedback",
            "--name",
            "pr-review-sweeper",
            "--yes",
        ],
    ));
    let job = serde_json::from_str::<Value>(&created).unwrap()["job"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let daemon = Daemon::start(home);
    let port = common::daemon::bound_port(home, "listener:gateway.http");

    let (status, answer) = post(port, "pull_request_review", "d-1", &review(42));
    assert_eq!(status, 202, "{answer}");
    assert_eq!(answer["status"], "accepted");
    assert_eq!(answer["job_id"], job.as_str());
    // A second event while the run is active is dropped.
    let (status, burst) = post(port, "pull_request_review", "d-2", &review(43));
    assert_eq!(status, 200, "{burst}");
    assert_eq!(burst["status"], "dropped");

    let mut done = Vec::new();
    wait("the run finishes", Duration::from_mins(2), || {
        done = runs(home, &job);
        done.first()
            .is_some_and(|r| !matches!(r["status"].as_str(), Some("queued" | "running")))
    });
    assert_eq!(done.len(), 1, "one run: {done:?}");
    assert_eq!(done[0]["trigger"], "webhook");
    assert_eq!(done[0]["status"], "ok", "{}", done[0]);
    assert_eq!(done[0]["output"], "swept PR 42");
    // A paused job drops its events too.
    ok(&run(home, &["cron", "pause", &job]));
    let (status, paused) = post(port, "pull_request_review", "d-3", &review(44));
    assert_eq!(
        (status, &paused["status"]),
        (200, &Value::from("dropped")),
        "{paused}"
    );
    assert_eq!(runs(home, &job).len(), 1);
    daemon.stop(home);
}
