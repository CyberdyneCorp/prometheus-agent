//! What reaches and leaves the API server of a running daemon (spec:
//! api-server, Run events stream; Sessions REST API; Chat Completions
//! endpoint): every tool preview passes forced secret redaction, a
//! caller's image media type never names a file, and an anonymous client
//! is refused before its body is read.

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::time::Duration;

use serde_json::{Value, json};

use self::common::api::{self, Request, chat};
use self::common::{append_config, daemon, ok, run};

const VALUE: &str = "horse-battery-staple-9417";

/// A home whose known secret `TICKET_TOKEN` is `VALUE`, with redaction
/// switched off, where the model runs `echo` of the secret.
fn secret_home() -> tempfile::TempDir {
    let home = api::home("");
    ok(&run(
        home.path(),
        &["config", "set", "security.redact_secrets", "false"],
    ));
    ok(&common::run_with_stdin(
        home.path(),
        &["secrets", "set", "TICKET_TOKEN"],
        &format!("{VALUE}\n"),
    ));
    let script = home.path().join("api-script.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\n\
             arguments = {{ command = \"echo token {VALUE}\" }}\n\
             [[rule]]\ntext = \"echoed\"\nrepeat = true\n"
        ),
    )
    .unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            script.to_string_lossy()
        ),
    );
    home
}

fn assert_redacted(preview: &Value, all: &str) {
    let preview = preview.as_str().unwrap_or_else(|| panic!("{all}"));
    assert!(preview.contains("[REDACTED:TICKET_TOKEN]"), "{all}");
    assert!(!preview.contains(VALUE), "{all}");
}

/// Session chat stream: `tool.started` and `tool.completed` previews are
/// redacted even with `security.redact_secrets = false`.
#[test]
fn session_chat_previews_redact_known_secrets() {
    let home = secret_home();
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let created = Request::new("POST", "/api/sessions")
        .auth("default")
        .json(&json!({}))
        .send(port);
    assert_eq!(created.status, 201, "{}", created.body);
    let id = created.json()["id"].as_str().unwrap().to_owned();
    let streamed = Request::new("POST", &format!("/api/sessions/{id}/chat/stream"))
        .auth("default")
        .json(&json!({"message": "echo the token"}))
        .send(port);
    let events = streamed.events();
    for name in ["tool.started", "tool.completed"] {
        let (_, _, data) = events
            .iter()
            .find(|(_, e, _)| e.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no {name}: {events:?}"));
        let data: Value = serde_json::from_str(data).unwrap();
        assert_redacted(&data["preview"], &data.to_string());
    }
}

/// Chat Completions stream: the tool progress preview is redacted.
#[test]
fn chat_completion_progress_previews_redact_known_secrets() {
    let home = secret_home();
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = chat(
        port,
        "default",
        &json!({"stream": true, "messages": [{"role": "user", "content": "echo the token"}]}),
    );
    let events = answer.events();
    let (_, _, data) = events
        .iter()
        .find(|(_, e, _)| e.as_deref() == Some("prometheus.tool.progress"))
        .unwrap_or_else(|| panic!("no tool progress: {events:?}"));
    let data: Value = serde_json::from_str(data).unwrap();
    assert_redacted(&data["preview"], &data.to_string());
}

/// A data URL whose media type climbs out of the image cache is refused
/// with `unsupported_content_type` and nothing is written.
#[test]
fn an_image_media_type_never_names_a_file() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let profile = home.path().join("profiles/default");
    for media_type in [
        "image/../../../escaped",
        "image/png:stream",
        "image/svg+xml",
    ] {
        let answer = chat(
            port,
            "default",
            &json!({"messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,iVBORw==")}},
            ]}]}),
        );
        assert_eq!(answer.status, 400, "{media_type}: {}", answer.body);
        assert_eq!(
            answer.json()["error"]["code"],
            "unsupported_content_type",
            "{media_type}"
        );
    }
    assert!(!profile.join("escaped").exists());
    assert!(!profile.join("cache/escaped").exists());
    let cache = profile.join("cache/api-images");
    assert!(
        !cache.exists() || std::fs::read_dir(&cache).unwrap().next().is_none(),
        "no image was stored"
    );
}

/// An anonymous request announcing a large body is refused at once,
/// without the daemon waiting for (or buffering) the body.
#[test]
fn an_unauthenticated_upload_is_refused_before_its_body() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let answer = refused_without_body(port);
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
}

/// Sends the head of a 16 MiB upload with a wrong key and no body; the
/// answer that arrives anyway.
fn refused_without_body(port: u16) -> String {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Authorization: Bearer wrong\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n",
        16 * 1024 * 1024
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut answer = vec![0_u8; 4096];
    let read = stream.read(&mut answer).expect("an answer before the body");
    String::from_utf8_lossy(&answer[..read]).into_owned()
}
