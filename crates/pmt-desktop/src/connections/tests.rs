//! The registry's rules (spec: desktop-app, Connection registry; Remote
//! connections are authenticated and encrypted; Gateway switching and
//! scoping).

use super::{Draft, Kind, LOCAL, OnLaunch, Registry, SshTarget};

fn remote(name: &str, url: &str) -> Draft {
    Draft {
        id: None,
        name: name.into(),
        kind: Kind::Remote,
        url: Some(url.into()),
        ssh: None,
    }
}

fn ssh(name: &str, host: &str, profile: Option<&str>) -> Draft {
    Draft {
        id: None,
        name: name.into(),
        kind: Kind::Ssh,
        url: None,
        ssh: Some(SshTarget {
            user: Some("me".into()),
            host: host.into(),
            port: 22,
            profile: profile.map(str::to_owned),
            binary: None,
        }),
    }
}

fn registry() -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(dir.path());
    (dir, registry)
}

/// Scenario: Duplicate URL.
#[test]
fn a_second_connection_to_the_same_normalized_url_is_refused() {
    let (_dir, mut r) = registry();
    r.save(&remote("Homelab", "https://box.example/")).unwrap();
    let refused = r.save(&remote("Other", "https://BOX.example")).unwrap_err();
    assert_eq!(
        refused,
        "a connection to https://box.example already exists (Homelab)"
    );
}

/// Scenario: Plain HTTP to a LAN host.
#[test]
fn plain_http_is_refused_except_to_loopback() {
    let (_dir, mut r) = registry();
    let refused = r
        .save(&remote("LAN", "http://192.168.1.5:9120"))
        .unwrap_err();
    assert_eq!(
        refused,
        "remote connections require https (or an ssh connection)"
    );
    r.save(&remote("Here", "http://127.0.0.1:9120")).unwrap();
}

/// Scenario: Plugin lists connections.
#[test]
fn plugins_see_names_kinds_and_the_primary_only() {
    let (_dir, mut r) = registry();
    let saved = r.save(&remote("Homelab", "https://box.example")).unwrap();
    r.set_primary(&saved.id).unwrap();
    let seen = r.for_plugins().to_string();
    assert!(
        seen.contains("Homelab") && seen.contains("\"remote\""),
        "{seen}"
    );
    assert!(
        seen.contains(&format!("\"primary\":\"{}\"", saved.id)),
        "{seen}"
    );
    assert!(!seen.contains("box.example"), "no URL or host: {seen}");
}

#[test]
fn names_are_unique_ignoring_case_and_bounded() {
    let (_dir, mut r) = registry();
    r.save(&remote("Homelab", "https://a.example")).unwrap();
    let refused = r.save(&remote("HOMELAB", "https://b.example")).unwrap_err();
    assert!(refused.contains("already exists"), "{refused}");
    let long = "x".repeat(65);
    assert!(r.save(&remote(&long, "https://c.example")).is_err());
}

#[test]
fn ssh_connections_are_unique_per_address_and_remote_profile() {
    let (_dir, mut r) = registry();
    r.save(&ssh("A", "box", None)).unwrap();
    r.save(&ssh("B", "box", Some("research"))).unwrap();
    let refused = r.save(&ssh("C", "BOX", None)).unwrap_err();
    assert_eq!(refused, "a connection to me@BOX:22 already exists (A)");
}

#[test]
fn the_local_connection_stays_and_a_removed_primary_falls_back_to_it() {
    let (dir, mut r) = registry();
    assert!(r.remove(LOCAL).is_err());
    let saved = r.save(&remote("Homelab", "https://box.example")).unwrap();
    r.set_primary(&saved.id).unwrap();
    r.remove(&saved.id).unwrap();
    assert_eq!(r.listing().primary, LOCAL);
    // Saved to disk: names, kinds, URLs and hosts only.
    let reopened = Registry::open(dir.path());
    assert_eq!(reopened.listing().connections.len(), 1);
}

/// Scenario: Failed switch is not remembered.
#[test]
fn launch_opens_the_last_connection_that_succeeded() {
    let (dir, mut r) = registry();
    let lab = r.save(&remote("Homelab", "https://lab.example")).unwrap();
    let vps = r.save(&remote("VPS", "https://vps.example")).unwrap();
    r.set_on_launch(OnLaunch::LastUsed).unwrap();
    r.connected(&lab.id).unwrap();
    // The switch to VPS failed: nothing reports it connected.
    let _ = vps;
    let restarted = Registry::open(dir.path());
    assert_eq!(restarted.launch(), lab.id);
    let mut primary = restarted;
    primary.set_on_launch(OnLaunch::Primary).unwrap();
    assert_eq!(primary.launch(), LOCAL);
}
