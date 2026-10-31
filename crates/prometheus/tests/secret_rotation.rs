//! A key rotated with `prometheus secrets set` in another terminal reaches
//! the next provider request of a running session (spec: configuration-
//! and-profiles, Secrets are resolved per use, scenario "Rotated key used
//! next request"). The provider is a local mock HTTP server that records
//! the `Authorization` header of each request.

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use common::{ok, run_with_stdin, temp_home};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use prometheus::slash_handlers::reload_secrets;

/// Serves `requests` requests, sending each `Authorization` header back.
fn mock_provider(requests: usize) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(requests) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("Authorization: ") {
                    value.trim().clone_into(&mut auth);
                }
            }
            tx.send(auth).unwrap();
            let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (addr, rx)
}

/// One provider request authenticated with the secret resolved now.
fn provider_request(scope: &ProfileScope, addr: &str) {
    let key = scope.secrets().resolve("OPENAI_API_KEY").unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        key.expose()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[test]
fn rotated_key_is_used_by_the_next_request() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-old-1111111111",
    ));
    // The running session: one scope for its whole lifetime.
    let launch = LaunchEnv::from_vars(std::iter::empty(), &EnvPolicy::default());
    let profile = Home::open(home.path())
        .unwrap()
        .existing_profile(&ProfileName::default_profile())
        .unwrap();
    let scope =
        ProfileScope::open(profile, &Overrides::default(), Launch::Profile(&launch)).unwrap();
    let (addr, seen) = mock_provider(2);

    provider_request(&scope, &addr);
    assert_eq!(seen.recv().unwrap(), "Bearer sk-old-1111111111");

    // Another terminal rotates the key, then the user runs /reload-secrets.
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-new-2222222222",
    ));
    let reloaded = reload_secrets(&scope).unwrap();
    assert_eq!(reloaded, "secrets reloaded; changed: OPENAI_API_KEY");
    assert!(!reloaded.contains("sk-"));

    provider_request(&scope, &addr);
    assert_eq!(seen.recv().unwrap(), "Bearer sk-new-2222222222");
}
