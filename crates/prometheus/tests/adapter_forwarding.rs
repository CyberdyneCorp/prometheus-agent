//! Adapter host forwarding mode (spec: platform-adapters, Adapter host
//! forwarding mode): `daemon run --adapters-only --upstream` runs only the
//! adapters and forwards every admitted event to an upstream daemon over
//! its remote attachment listener, which owns the sessions; the upstream's
//! reply goes out through the local adapter. Two homes stand for the host
//! (the `test` file adapter is its platform) and the container daemon.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
use self::common::{daemon, mock_home, ok, run, run_with_stdin};

/// The upstream: answers `upstream answered`, remote listener on, with an
/// admin device paired for the host; returns its token.
fn upstream() -> (common::TestHome, String) {
    let home = mock_home();
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("up.toml"),
        "[[rule]]\nrepeat = true\ntext = \"upstream answered\"\n",
    )
    .unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"up.toml\"\n\n[gateway.busy]\ndebounce_ms = 0\n",
    );
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon.remote]\nenabled = true\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    let paired = ok(&run(
        home.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "matrix-host",
            "--role",
            "admin",
            "--profile-binding",
            "default",
        ],
    ));
    let token = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    (home, token)
}

/// The adapter host: would answer `host answered` itself; trusts the
/// upstream's self-signed certificate and holds `token`.
fn adapter_host(upstream: &Path, token: &str) -> common::TestHome {
    let cert = upstream.join("run/remote-cert.pem");
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"host answered\"\n",
        &format!(
            "\n[network]\nextra_ca_file = {}\n",
            toml_string(&cert.to_string_lossy())
        ),
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "UPSTREAM_TICKET"],
        &format!("{token}\n"),
    ));
    home
}

fn toml_string(text: &str) -> String {
    format!("'{text}'")
}

fn sessions(home: &Path) -> Vec<Value> {
    let listed = ok(&run(
        home,
        &["--json", "sessions", "list", "--all", "--limit", "100"],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"].as_array().cloned().unwrap_or_default()
}

fn adapter_state(home: &Path) -> String {
    ok(&run(home, &["--json", "gateway", "status"]))
}

/// Scenario: Container agent with host crypto (the host's adapter
/// receives, the upstream answers, the host sends).
#[test]
fn an_adapters_only_host_forwards_and_delivers_the_upstream_reply() {
    let (up, token) = upstream();
    let _up_daemon = daemon::start(up.path(), &[]);
    let port = daemon::bound_port(up.path(), "listener:remote");
    let host = adapter_host(up.path(), &token);
    let url = format!("wss://localhost:{port}/adapters");
    let host_daemon = Daemon::start_with(host.path(), &["--adapters-only", "--upstream", &url]);
    // Up: `connected`, or a fresh connection not yet confirmed by a send.
    wait("the host adapter is up", Duration::from_mins(1), || {
        let status = adapter_state(host.path());
        (status.contains("\"connected\"") || status.contains("\"unconfirmed\""))
            && !status.contains("upstream_unreachable")
    });
    send(host.path(), "default", "alice", "hello");
    wait_said(host.path(), "default", "upstream answered");
    assert!(
        !texts(host.path(), "default")
            .iter()
            .any(|t| t.contains("host answered"))
    );
    host_daemon.stop(host.path());
    // Sessions are the upstream's only.
    assert!(sessions(host.path()).is_empty());
    let owned = sessions(up.path());
    assert_eq!(owned.len(), 1, "{owned:?}");
}

/// An unreachable upstream puts the host's adapters into `degraded` and a
/// person writing in is told the agent is unavailable; nothing runs here.
#[test]
fn an_unreachable_upstream_degrades_the_hosts_adapters() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/adapters", closed.local_addr().unwrap());
    drop(closed);
    let host = gateway_home_with("[[rule]]\nrepeat = true\ntext = \"host answered\"\n", "");
    ok(&run_with_stdin(
        host.path(),
        &["secrets", "set", "UPSTREAM_TICKET"],
        "pmtd_unused\n",
    ));
    let host_daemon = Daemon::start_with(host.path(), &["--adapters-only", "--upstream", &url]);
    wait("the adapter is degraded", Duration::from_mins(1), || {
        let status = adapter_state(host.path());
        status.contains("\"degraded\"") && status.contains("upstream_unreachable")
    });
    send(host.path(), "default", "alice", "hello");
    wait_said(
        host.path(),
        "default",
        "Upstream agent unavailable, try again shortly.",
    );
    host_daemon.stop(host.path());
    assert!(sessions(host.path()).is_empty());
}

/// Only an admin device may attach as an adapter host: it vouches for the
/// senders it forwards.
#[test]
fn a_non_admin_device_cannot_attach_as_an_adapter_host() {
    let (up, _) = upstream();
    let paired = ok(&run(
        up.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "laptop",
            "--profile-binding",
            "default",
        ],
    ));
    let driver = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let _up_daemon = daemon::start(up.path(), &[]);
    let port = daemon::bound_port(up.path(), "listener:remote");
    let host = adapter_host(up.path(), &driver);
    let url = format!("wss://localhost:{port}/adapters");
    let _host_daemon = Daemon::start_with(host.path(), &["--adapters-only", "--upstream", &url]);
    // Degraded from the start ("not connected yet"); wait for the
    // upstream's refusal so the message is not sent mid-handshake.
    wait(
        "the upstream refused the host",
        Duration::from_mins(1),
        || {
            let status = adapter_state(host.path());
            status.contains("upstream_unreachable") && !status.contains("not connected yet")
        },
    );
    send(host.path(), "default", "alice", "hello");
    wait_said(
        host.path(),
        "default",
        "Upstream agent unavailable, try again shortly.",
    );
    assert!(adapter_state(host.path()).contains("upstream_unreachable"));
    assert!(sessions(up.path()).is_empty());
}
