//! The server set following `mcp.servers` (spec: plugin-system, MCP config
//! watch, recycling and discovery wait).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_mcp::server::State;

fn scope_with(root: &std::path::Path, servers: &[&str]) -> ProfileScope {
    let home = Home::open(root).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let fixture = pmt_mcp::testkit::fixture();
    let mut text = String::new();
    for name in servers {
        let _ = write!(
            text,
            "[mcp.servers.{name}]\ncommand = {:?}\ntrust = \"full\"\n\n",
            fixture.to_string_lossy()
        );
    }
    std::fs::write(profile.path().join("config.toml"), text).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

#[tokio::test]
async fn a_config_change_keeps_unchanged_servers_and_starts_added_ones() {
    let root = tempfile::tempdir().unwrap();
    let first = scope_with(root.path(), &["a"]);
    let manager = super::manager(&first);
    manager.discover().await;
    let a = Arc::clone(manager.server("a").unwrap());
    assert_eq!(a.state(), State::Connected);

    let second = scope_with(root.path(), &["a", "b"]);
    super::config_changed(&second);
    let manager = super::manager(&second);
    assert!(
        Arc::ptr_eq(manager.server("a").unwrap(), &a),
        "unchanged server kept"
    );
    assert_eq!(a.state(), State::Connected, "and its connection");
    let b = Arc::clone(manager.server("b").unwrap());
    for _ in 0..100 {
        if b.state() == State::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        b.state(),
        State::Connected,
        "an added server starts without a session"
    );

    let third = scope_with(root.path(), &["b"]);
    super::config_changed(&third);
    assert!(super::manager(&third).server("a").is_none());
    assert_eq!(a.state(), State::NotConnected, "a removed server stops");
    assert!(Arc::ptr_eq(super::manager(&third).server("b").unwrap(), &b));
}
