//! The Langfuse preset and the `NeMo` Relay files (spec: observability,
//! `NeMo` Relay and Langfuse integrations): a daemon whose preset lacks a
//! Langfuse key refuses to start naming it; with both keys a session's
//! spans reach `<host>/api/public/otel/v1/traces` with Basic
//! authentication; `doctor observability` lists the Relay files.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::{append_config, command, mock_home, ok, run, run_with_stdin, stderr};

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A fake Langfuse: 200 to every request, its path and Authorization kept.
fn langfuse() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let keep = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let keep = Arc::clone(&keep);
            std::thread::spawn(move || serve(stream, &keep));
        }
    });
    (url, seen)
}

/// Answers every request of one connection.
fn serve(stream: std::net::TcpStream, keep: &Seen) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    while let Some(request) = request(&mut reader) {
        keep.lock().unwrap().push(request);
        if writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .is_err()
        {
            return;
        }
    }
}

/// One request's path and Authorization (its body read and dropped).
fn request(reader: &mut BufReader<std::net::TcpStream>) -> Option<(String, Option<String>)> {
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let (mut auth, mut length) = (None, 0usize);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let (name, value) = header.trim_end().split_once(':').unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "" => break,
            "authorization" => auth = Some(value.trim().to_owned()),
            "content-length" => length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    std::io::Read::read_exact(reader, &mut body).ok()?;
    Some((path, auth))
}

fn secret(home: &std::path::Path, name: &str, value: &str) {
    ok(&run_with_stdin(
        home,
        &["secrets", "set", name],
        &format!("{value}\n"),
    ));
}

#[test]
fn a_langfuse_preset_without_its_secret_key_stops_the_daemon_naming_it() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[observability.otel]\npreset = \"langfuse\"\n",
    );
    secret(home.path(), "LANGFUSE_PUBLIC_KEY", "pk-lf-1");
    let out = run(home.path(), &["daemon", "run"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("LANGFUSE_SECRET_KEY"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn langfuse_receives_the_spans_with_basic_authentication() {
    let home = mock_home();
    let (url, seen) = langfuse();
    append_config(
        home.path(),
        &format!(
            "\n[observability.otel]\npreset = \"langfuse\"\n\n[observability.langfuse]\nhost = \"{url}\"\n"
        ),
    );
    secret(home.path(), "LANGFUSE_PUBLIC_KEY", "pk-lf-1");
    secret(home.path(), "LANGFUSE_SECRET_KEY", "sk-lf-2");
    ok(&command(home.path())
        .current_dir(home.path())
        .args(["-z", "hello"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap());
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("pk-lf-1:sk-lf-2")
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|(path, auth)| path == "/api/public/otel/v1/traces"
                && auth.as_deref() == Some(expected.as_str())),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .all(|(path, _)| path != "/api/public/otel/v1/metrics"),
        "Langfuse takes traces only: {seen:?}"
    );
}

#[test]
fn doctor_lists_the_relay_files_with_the_system_one_last() {
    let home = mock_home();
    let user = common::user_home(home.path());
    let user_file = user.join(".config/nemo-relay/plugins.toml");
    std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
    std::fs::write(&user_file, "version = 1\n").unwrap();
    let system = home.path().join("etc-nemo-relay");
    std::fs::create_dir_all(&system).unwrap();
    std::fs::write(system.join("plugins.toml"), "version = 1\n").unwrap();
    let out = command(home.path())
        .args(["doctor", "observability"])
        .env("PMT_TEST_RELAY_SYSTEM_DIR", &system)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let user_at = text.find(&user_file.display().to_string()).expect(&text);
    let system_line = format!(
        "{} (system, highest precedence)",
        system.join("plugins.toml").display()
    );
    let system_at = text.find(&system_line).expect(&text);
    assert!(user_at < system_at, "{text}");
}
