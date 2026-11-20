//! Gateway proxy mode (spec: api-server, Gateway proxy mode): a daemon
//! whose `gateway.proxy.url` names another daemon's API server keeps its
//! adapters and delivery but runs every admitted turn on that server, keyed
//! by the local conversation key. Two homes stand for the container and
//! the host; the `test` file adapter is the container's messaging platform.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
use self::common::{api, ok, run, run_with_stdin};

/// The host: a mock profile serving the API, answering `host answered`.
fn host() -> tempfile::TempDir {
    let home = api::home("");
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("host.toml"),
        "[[rule]]\nrepeat = true\ntext = \"host answered\"\n",
    )
    .unwrap();
    common::append_config(home.path(), "\n[providers.mock]\nscript = \"host.toml\"\n");
    home
}

/// The container: answers `container answered` itself, proxied to `url`
/// with `key` in `UPSTREAM_KEY`.
fn container(url: &str, key: &str) -> tempfile::TempDir {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"container answered\"\n",
        &format!("\n[gateway.proxy]\nurl = \"{url}\"\nkey_secret = \"UPSTREAM_KEY\"\n"),
    );
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "UPSTREAM_KEY"],
        &format!("{key}\n"),
    ));
    home
}

fn sessions(home: &Path) -> Vec<Value> {
    let listed = ok(&run(
        home,
        &["--json", "sessions", "list", "--all", "--limit", "100"],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"].as_array().cloned().unwrap_or_default()
}

/// Scenario: Container relays to host agent.
#[test]
fn a_proxied_chat_is_answered_by_the_host_agent() {
    let host = host();
    let host_daemon = common::daemon::start(host.path(), &[]);
    let port = api::port(host.path());
    let url = format!("http://127.0.0.1:{port}/p/default");
    let container = container(&url, &api::key_of("default"));
    let daemon = Daemon::start(container.path());
    send(container.path(), "default", "alice", "hello");
    wait_said(container.path(), "default", "host answered");
    // A second message continues the same remote session.
    send(container.path(), "default", "alice", "again");
    wait("two answers", Duration::from_mins(1), || {
        texts(container.path(), "default")
            .iter()
            .filter(|t| t.contains("host answered"))
            .count()
            == 2
    });
    assert!(
        !texts(container.path(), "default")
            .iter()
            .any(|t| t.contains("container answered"))
    );
    daemon.stop(container.path());
    // No local turn: the container stored no session.
    assert!(sessions(container.path()).is_empty());
    let remote: Vec<Value> = sessions(host.path())
        .into_iter()
        .filter(|s| s["source"] == "api")
        .collect();
    assert_eq!(remote.len(), 1, "{remote:?}");
    assert!(
        remote[0]["message_count"].as_u64().unwrap() >= 4,
        "{remote:?}"
    );
    drop(host_daemon);
}

#[test]
fn an_unreachable_upstream_answers_unavailable_and_queues_nothing() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/p/default", closed.local_addr().unwrap());
    drop(closed);
    let container = container(&url, "unused");
    let daemon = Daemon::start(container.path());
    send(container.path(), "default", "alice", "hello");
    wait_said(
        container.path(),
        "default",
        "Upstream agent unavailable, try again shortly.",
    );
    daemon.stop(container.path());
    assert!(sessions(container.path()).is_empty());
}

#[test]
fn a_refused_key_puts_the_adapters_in_fatal_with_proxy_auth() {
    let host = host();
    let host_daemon = common::daemon::start(host.path(), &[]);
    let url = format!("http://127.0.0.1:{}/p/default", api::port(host.path()));
    let container = container(&url, "wrong-key-0123456789");
    let _daemon = Daemon::start(container.path());
    send(container.path(), "default", "alice", "hello");
    wait("the adapter is fatal", Duration::from_mins(1), || {
        let status = ok(&run(container.path(), &["--json", "gateway", "status"]));
        status.contains("proxy_auth") && status.contains("fatal")
    });
    assert!(sessions(container.path()).is_empty());
    drop(host_daemon);
}
