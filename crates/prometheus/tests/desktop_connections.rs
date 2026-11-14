//! Remote and SSH connections of the desktop shell against real daemons
//! (spec: desktop-app, Connection registry; Remote connections are
//! authenticated and encrypted; SSH connections reuse the local ticket
//! model): a pinned self-signed dashboard listener reached with a paired
//! device credential, a certificate that changes, and a host reached
//! through a stand-in `ssh` whose host key is confirmed, then changes.
#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::connections::{
    self, HOST_KEY_A, HOST_KEY_B, ask, ask0, call, close, dial, fake_ssh, host_with_prometheus,
    set_host_key, ssh_runs,
};
use common::{daemon, dashboard, mock_home, ok, run, temp_home};
use pmt_desktop::connections::ssh::{
    HOST_KEY_CHANGED, NOT_INSTALLED, SshPrograms, host_key_fingerprint,
};
use serde_json::{Value, json};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// `prometheus serve` on 127.0.0.1 with a self-signed certificate.
fn serve_tls(home: &std::path::Path) -> std::process::Child {
    dashboard::certificate(home);
    std::fs::write(
        home.join("daemon.toml"),
        format!(
            "[dashboard.tls]\ncert = {:?}\nkey = {:?}\n",
            home.join("cert.pem").to_string_lossy(),
            home.join("key.pem").to_string_lossy()
        ),
    )
    .unwrap();
    common::command(home)
        .args(["serve", "--host", "127.0.0.1", "--port", "0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

/// Scenario: Duplicate URL (the shell's registry, through the page's
/// request) and the remote connection's whole path: the key is pinned
/// after comparing it with `dashboard --fingerprint`, the device is
/// paired with a code, and the page's dial reaches the remote daemon
/// through the shell with a one-use ticket while the credential stays in
/// the keychain.
#[test]
fn a_remote_connection_is_pinned_paired_and_dialed_through_the_shell() {
    let remote = mock_home();
    let mut server = serve_tls(remote.path());
    let port = daemon::bound_port(remote.path(), "listener:dashboard");
    let local = temp_home();
    let app = local.path().join("app");
    let (shell, keychain) = connections::shell(local.path(), &app, SshPrograms::default());
    let rt = runtime();
    rt.block_on(async {
        let url = format!("https://127.0.0.1:{port}/");
        let saved = ask(
            &shell,
            "connection_save",
            json!({"name": "Homelab", "kind": "remote", "url": url}),
        )
        .await
        .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        let again = ask(
            &shell,
            "connection_save",
            json!({"name": "Other", "kind": "remote", "url": format!("HTTPS://127.0.0.1:{port}")}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            again,
            format!("a connection to https://127.0.0.1:{port} already exists (Homelab)")
        );

        let first = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        let printed = ok(&run(remote.path(), &["dashboard", "--fingerprint"]));
        assert_eq!(first["confirm"]["kind"], "certificate", "{first}");
        assert_eq!(first["confirm"]["fingerprint"], printed.trim(), "{first}");
        ask(
            &shell,
            "connection_trust",
            json!({"id": id, "fingerprint": printed.trim()}),
        )
        .await
        .unwrap();
        let unpaired = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(unpaired["health"]["ok"], true, "{unpaired}");
        assert_eq!(unpaired["websocket"]["ok"], false, "{unpaired}");

        let code = ok(&run(
            remote.path(),
            &["--json", "dashboard", "pair", "--name", "desk"],
        ));
        let code = serde_json::from_str::<Value>(&code).unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        ask(&shell, "connection_pair", json!({"id": id, "code": code}))
            .await
            .unwrap();
        let paired = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(paired["websocket"]["ok"], true, "{paired}");

        let answer = ask(
            &shell,
            "prepare_dial",
            json!({"connection": id, "profile": "default"}),
        )
        .await
        .unwrap();
        assert!(
            answer["url"]
                .as_str()
                .unwrap()
                .starts_with("ws://127.0.0.1:"),
            "{answer}"
        );
        let mut ws = dial(&answer).await.unwrap();
        let listed = call(&mut ws, 1, "session.list", json!({"profile": "default"})).await;
        assert!(listed.get("result").is_some(), "{listed}");
        close(ws).await;
        assert!(
            dial(&answer).await.is_err(),
            "a forwarder ticket works once"
        );

        let listing = ask0(&shell, "connections_list").await.unwrap();
        assert_eq!(listing["paired"], json!([id]), "{listing}");
        let token = keychain.accounts();
        assert_eq!(token, [format!("desktop-connection/{id}")]);
        let file = std::fs::read_to_string(app.join("data/connections.json")).unwrap();
        assert!(
            file.contains("Homelab") && file.contains("127.0.0.1"),
            "{file}"
        );
        assert!(!listing.to_string().contains("token\""), "{listing}");
    });
    ok(&run(remote.path(), &["daemon", "stop"]));
    let _ = server.wait();
}

/// A TLS server presenting a new key written to `dir`; counts
/// connections. Runs on the calling runtime.
fn tls_server(
    port: u16,
    dir: &std::path::Path,
    accepted: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    std::fs::create_dir_all(dir).unwrap();
    dashboard::certificate(dir);
    let acceptor =
        pmt_gateway::cert::acceptor(&dir.join("cert.pem"), &dir.join("key.pem"), "test").unwrap();
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind(([127, 0, 0, 1], port).into()).unwrap();
    let listener = socket.listen(16).unwrap();
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            pmt_core::spawn::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buf = [0_u8; 4096];
                let _ = tls.read(&mut buf).await;
                let body = r#"{"version":"test"}"#;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(reply.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    })
}

/// Scenario: Certificate changed.
#[test]
fn a_changed_certificate_stops_the_dials_until_it_is_pinned_again() {
    let local = temp_home();
    let app = local.path().join("app");
    let (shell, _keychain) = connections::shell(local.path(), &app, SshPrograms::default());
    let rt = runtime();
    rt.block_on(async {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let first = tls_server(port, &local.path().join("a"), Arc::clone(&accepted));
        let saved = ask(&shell, "connection_save", json!({"name": "Homelab", "kind": "remote", "url": format!("https://127.0.0.1:{port}")}))
            .await
            .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        let shown = ask(&shell, "connection_test", json!({"id": id})).await.unwrap();
        let pinned = shown["confirm"]["fingerprint"].as_str().unwrap().to_owned();
        ask(&shell, "connection_trust", json!({"id": id, "fingerprint": pinned})).await.unwrap();
        assert_eq!(ask(&shell, "connection_test", json!({"id": id})).await.unwrap()["health"]["ok"], true);

        first.abort();
        let _ = first.await;
        let second = tls_server(port, &local.path().join("b"), Arc::clone(&accepted));
        let changed = ask(&shell, "connection_test", json!({"id": id})).await.unwrap();
        let latched = &changed["latched"];
        assert_eq!(latched["kind"], "certificate", "{changed}");
        assert_eq!(latched["pinned"], pinned.as_str(), "{changed}");
        let presented = latched["presented"].as_str().unwrap().to_owned();
        assert_ne!(presented, pinned);
        let message = latched["message"].as_str().unwrap();
        assert!(message.contains(&pinned) && message.contains(&presented), "{message}");

        let contacted = accepted.load(Ordering::SeqCst);
        let refused = ask(&shell, "prepare_dial", json!({"connection": id, "profile": "default"}))
            .await
            .unwrap_err();
        assert!(refused.contains("has changed"), "{refused}");
        let again = ask(&shell, "connection_test", json!({"id": id})).await.unwrap();
        assert_eq!(again["latched"]["kind"], "certificate");
        assert_eq!(accepted.load(Ordering::SeqCst), contacted, "no retry while latched");

        // Re-pin: the user compared the new key.
        ask(&shell, "connection_trust", json!({"id": id, "fingerprint": presented})).await.unwrap();
        let repinned = ask(&shell, "connection_test", json!({"id": id})).await.unwrap();
        assert_eq!(repinned["health"]["ok"], true, "{repinned}");
        assert!(accepted.load(Ordering::SeqCst) > contacted);
        second.abort();
    });
}

fn blob(key: &str) -> &str {
    key.split_whitespace().nth(1).unwrap()
}

/// Scenario: Host key changed (and the SSH path itself: the host key is
/// confirmed on first use, `daemon ensure` and `daemon ticket` run over
/// SSH, and the page's dial is carried by `ssh -W` to the remote daemon's
/// loopback endpoint).
#[test]
fn an_ssh_connection_confirms_its_host_key_and_latches_a_changed_one() {
    let remote = mock_home();
    let local = temp_home();
    let fake = local.path().join("fake-ssh");
    let programs = fake_ssh(&fake);
    host_with_prometheus(&fake, "lab.test", remote.path());
    let (shell, _keychain) = connections::shell(local.path(), &local.path().join("app"), programs);
    let rt = runtime();
    rt.block_on(async {
        let saved = ask(
            &shell,
            "connection_save",
            json!({"name": "Homelab", "kind": "ssh", "ssh": {"user": "me", "host": "lab.test"}}),
        )
        .await
        .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        let first = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        let key_a = host_key_fingerprint(blob(HOST_KEY_A)).unwrap();
        assert_eq!(
            first["confirm"],
            json!({"kind": "host_key", "fingerprint": key_a}),
            "{first}"
        );
        ask(
            &shell,
            "connection_trust",
            json!({"id": id, "fingerprint": key_a}),
        )
        .await
        .unwrap();
        let tested = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(tested["health"]["ok"], true, "{tested}");
        assert_eq!(tested["websocket"]["ok"], true, "{tested}");

        let answer = ask(
            &shell,
            "prepare_dial",
            json!({"connection": id, "profile": "default"}),
        )
        .await
        .unwrap();
        let mut ws = dial(&answer).await.unwrap();
        let listed = call(&mut ws, 1, "session.list", json!({"profile": "default"})).await;
        assert!(listed.get("result").is_some(), "{listed}");
        close(ws).await;

        set_host_key(&fake, HOST_KEY_B);
        let key_b = host_key_fingerprint(blob(HOST_KEY_B)).unwrap();
        let refused = ask(
            &shell,
            "prepare_dial",
            json!({"connection": id, "profile": "default"}),
        )
        .await
        .unwrap_err();
        assert!(
            refused.contains(HOST_KEY_CHANGED) && refused.contains(&key_b),
            "{refused}"
        );
        let runs = ssh_runs(&fake);
        let again = ask(
            &shell,
            "prepare_dial",
            json!({"connection": id, "profile": "default"}),
        )
        .await
        .unwrap_err();
        assert_eq!(again, refused);
        let latched = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(
            latched["latched"]["fingerprint"],
            key_b.as_str(),
            "{latched}"
        );
        assert_eq!(ssh_runs(&fake), runs, "no retry until the user acts");
    });
    ok(&run(remote.path(), &["daemon", "stop"]));
}

/// Scenario: Remote binary missing.
#[test]
fn test_reports_a_host_without_prometheus() {
    let local = temp_home();
    let fake = local.path().join("fake-ssh");
    let programs = fake_ssh(&fake);
    std::fs::create_dir_all(fake.join("hosts/bare.test")).unwrap();
    let (shell, _keychain) = connections::shell(local.path(), &local.path().join("app"), programs);
    runtime().block_on(async {
        let saved = ask(
            &shell,
            "connection_save",
            json!({"name": "Bare", "kind": "ssh", "ssh": {"host": "bare.test"}}),
        )
        .await
        .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        let key = host_key_fingerprint(blob(HOST_KEY_A)).unwrap();
        ask(
            &shell,
            "connection_trust",
            json!({"id": id, "fingerprint": key}),
        )
        .await
        .unwrap();
        let tested = ask(&shell, "connection_test", json!({"id": id}))
            .await
            .unwrap();
        let detail = tested["health"]["detail"].as_str().unwrap();
        assert_eq!(tested["health"]["ok"], false);
        assert!(detail.starts_with(NOT_INSTALLED), "{detail}");
        assert!(detail.contains("remote binary path"), "{detail}");
    });
}
