//! `prometheus debug share`, `debug delete` and `/debug` against a local
//! share endpoint (spec: observability, Opt-in debug share upload).

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use common::{append_config, command, mock_home, ok, run, stderr};

/// One request a [`Endpoint`] received.
#[derive(Debug, Clone)]
struct Request {
    /// `METHOD /path`.
    line: String,
    /// Header lines, lowercased.
    headers: Vec<String>,
    body: String,
}

/// A local HTTP endpoint that keeps every request and answers an upload
/// with `{"url": "<its own url>/r/1", "delete_token": "tok"}`.
struct Endpoint {
    url: String,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl Endpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Request>>> = Arc::default();
        let keep = Arc::clone(&seen);
        let answer = format!(r#"{{"url":"{url}/r/1","delete_token":"tok"}}"#);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer_one(stream, &keep, &answer);
            }
        });
        Self { url, seen }
    }

    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

fn answer_one(stream: std::net::TcpStream, keep: &Mutex<Vec<Request>>, answer: &str) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut head = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
            break;
        }
        head.push(header.trim_end().to_ascii_lowercase());
    }
    let length = head
        .iter()
        .find_map(|h| h.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap_or(0));
    let mut body = vec![0; length];
    let _ = std::io::Read::read_exact(&mut reader, &mut body);
    keep.lock().unwrap().push(Request {
        line: line
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "),
        headers: head,
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
        answer.len()
    );
    let _ = writer.write_all(response.as_bytes());
}

fn shares(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("logs/shares.json")).unwrap_or_default()
}

#[test]
fn a_custom_share_sends_the_header_secrets_and_delete_removes_it() {
    let home = mock_home();
    let endpoint = Endpoint::start();
    append_config(
        home.path(),
        &format!(
            "\n[debug.share]\nendpoint = \"{}/reports\"\nheader_secrets = [\"SUPPORT_KEY\"]\n",
            endpoint.url
        ),
    );
    let share = |args: &[&str]| {
        command(home.path())
            .env("PMT_SECRET_SUPPORT_KEY", "support-0123456789")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let text = ok(&share(&["debug", "share", "--target", "custom", "--yes"]));
    let url = format!("{}/r/1", endpoint.url);
    assert!(text.contains(&format!("uploaded: {url}")), "{text}");
    let upload = &endpoint.requests()[0];
    assert_eq!(upload.line, "POST /reports");
    assert!(upload.body.contains("version"), "{upload:?}");
    assert!(
        upload
            .headers
            .contains(&"support-key: support-0123456789".to_owned()),
        "{upload:?}"
    );
    assert!(shares(home.path()).contains("\"delete_token\": \"tok\""));

    let text = ok(&share(&["debug", "delete", &url]));
    assert_eq!(text.trim(), format!("deleted {url}"));
    let delete = &endpoint.requests()[1];
    assert_eq!(delete.line, "DELETE /r/1");
    assert!(
        delete.headers.contains(&"x-delete-token: tok".to_owned()),
        "{delete:?}"
    );
    assert!(
        delete
            .headers
            .contains(&"support-key: support-0123456789".to_owned()),
        "{delete:?}"
    );
    assert!(
        !shares(home.path()).contains(&url),
        "{}",
        shares(home.path())
    );
    // A share that is no longer recorded cannot be deleted again.
    let again = share(&["debug", "delete", &url]);
    assert_ne!(again.status.code(), Some(0));
    assert!(
        stderr(&again).contains("not a recorded share"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn a_paste_rs_share_is_deleted_with_a_delete_request() {
    let home = mock_home();
    let endpoint = Endpoint::start();
    let paste = format!("{}/", endpoint.url);
    let with_seam = |args: &[&str]| {
        command(home.path())
            .env("PMT_TEST_PASTE_URL", &paste)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    ok(&with_seam(&["debug", "share", "--yes"]));
    let url = format!("{}/r/1", endpoint.url);
    assert!(shares(home.path()).contains(&url));
    ok(&with_seam(&["debug", "delete", &url]));
    let requests = endpoint.requests();
    assert_eq!(requests[0].line, "POST /");
    assert_eq!(requests[1].line, "DELETE /r/1");
    assert!(!shares(home.path()).contains(&url));
}

/// `--no-redact` asks a second time; a no there uploads nothing.
#[cfg(unix)]
#[test]
fn an_unredacted_share_needs_a_second_yes() {
    use expectrl::{Eof, Expect};
    let home = mock_home();
    let endpoint = Endpoint::start();
    append_config(
        home.path(),
        &format!("\n[debug.share]\nendpoint = \"{}/reports\"\n", endpoint.url),
    );
    let args = ["debug", "share", "--target", "custom", "--no-redact"];
    let mut p = common::pty::spawn(home.path(), &args);
    p.expect("[y/N]").unwrap();
    p.send_line("y").unwrap();
    p.expect("is NOT redacted and may contain secrets. Upload it anyway? [y/N]")
        .unwrap();
    p.send_line("n").unwrap();
    p.expect("nothing was uploaded").unwrap();
    p.expect(Eof).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(endpoint.requests().is_empty(), "{:?}", endpoint.requests());

    let mut p = common::pty::spawn(home.path(), &args);
    p.expect("[y/N]").unwrap();
    p.send_line("y").unwrap();
    p.expect("Upload it anyway? [y/N]").unwrap();
    p.send_line("y").unwrap();
    p.expect("uploaded: ").unwrap();
    p.expect(Eof).unwrap();
    assert_eq!(endpoint.requests().len(), 1);
    // Never unattended, even with --yes.
    let out = run(
        home.path(),
        &[
            "debug",
            "share",
            "--target",
            "custom",
            "--no-redact",
            "--yes",
        ],
    );
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert_eq!(endpoint.requests().len(), 1);
}

/// `/debug` writes the report only; `/debug share` shows it and asks;
/// only `/debug share yes` uploads, once.
#[cfg(feature = "gateway")]
#[test]
fn debug_slash_command_asks_before_uploading() {
    use pmt_rpc::contracts as c;
    let home = mock_home();
    let endpoint = Endpoint::start();
    let started = command(home.path())
        .env("PMT_TEST_PASTE_URL", format!("{}/", endpoint.url))
        .args(["daemon", "start"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(ok(&started).starts_with("daemon started (pid "));
    let _daemon = common::daemon::Daemon {
        home: home.path(),
        pid: common::daemon::record(home.path()).process.pid,
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (client, _inbox) = runtime.block_on(async {
        let mut hello = common::daemon::hello(false);
        hello.client = c::ClientKind::Tui;
        pmt_rpc::client::Client::connect(
            &common::daemon::record(home.path()).endpoint,
            pmt_rpc::control::Purpose::Interactive,
            vec!["default".into()],
            &hello,
        )
        .await
        .unwrap()
    });
    let id = runtime.block_on(async {
        let created = client
            .call("session.create", serde_json::json!({}))
            .await
            .unwrap();
        created["session_id"].as_str().unwrap().to_owned()
    });
    // The reply's notice, or the reason the command was refused.
    let call = |args: &str| {
        let reply = runtime
            .block_on(client.call(
                "command.dispatch",
                serde_json::json!({"session_id": id, "name": "debug", "args": args}),
            ))
            .unwrap();
        reply["notice"]
            .as_str()
            .or_else(|| reply["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let local = call("");
    assert!(local.contains("nothing was uploaded"), "{local}");
    let asked = call("share");
    let host = endpoint.url.trim_start_matches("http://");
    let host = host.split(':').next().unwrap();
    assert!(
        asked.contains(&format!(
            "\nUpload this report to {host}? It will be public to anyone with the link.\n\
             Send /debug share yes to upload; nothing is uploaded otherwise."
        )),
        "{asked}"
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(endpoint.requests().is_empty(), "{:?}", endpoint.requests());
    let uploaded = call("share yes");
    assert!(
        uploaded.contains(&format!("uploaded: {}/r/1", endpoint.url)),
        "{uploaded}"
    );
    assert_eq!(endpoint.requests().len(), 1);
    assert!(shares(home.path()).contains("/r/1"));
    // The answer is used up: a second yes uploads nothing.
    assert!(
        call("share yes").contains("no report waits for an answer"),
        "a second yes"
    );
    assert_eq!(endpoint.requests().len(), 1);
}
