//! Local browser isolation (spec: execution-environments, Local browser
//! isolation): a local browser writes only its throwaway profile and temp
//! directories and reaches the network only through the session's egress
//! proxy, enforced by the OS sandbox; without the `browser.real_profile`
//! grant a session's browser runs on a throwaway profile.
//!
//! A stand-in browser probes the sandbox from inside (a real browser
//! always uses its proxy, so it cannot show the OS layer).

use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use pmt_config::schema::SandboxConfig;
use pmt_tools::browser::records::Record;

use crate::support::{self, Fixture};

/// Tries a write outside its directories and a direct connection that
/// skips the proxy, records both outcomes in its profile, then answers
/// the first DevTools call.
const PROBING_BROWSER: &str = r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in --user-data-dir=*) dir="${arg#--user-data-dir=}" ;; esac
done
if printf x > "$OUTSIDE/escaped" 2>/dev/null; then echo wrote > "$dir/probe"; else echo refused > "$dir/probe"; fi
if curl --noproxy '*' -s -m 3 -o /dev/null "http://127.0.0.1:$DIRECT_PORT/"; then echo reached >> "$dir/probe"; else echo blocked >> "$dir/probe"; fi
printf x > "$dir/written" && printf x > "$TMPDIR/written"
dd bs=1 count=1 <&3 >/dev/null 2>&1
printf '{"id":1,"result":{}}\000' >&4
sleep 600
"#;

#[tokio::test]
async fn a_local_browser_writes_only_its_own_directories_and_goes_through_the_proxy() {
    let fixture = Fixture::new();
    let bin = fixture.home.join("bin");
    let outside = fixture.home.join("outside");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let fake = bin.join("probing-browser");
    std::fs::write(&fake, PROBING_BROWSER).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A local service the browser must not reach around its proxy.
    let service = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    service.set_nonblocking(true).unwrap();
    let port = service.local_addr().unwrap().port();
    let mut env = fixture.env();
    env.set("OUTSIDE", &outside);
    env.set("DIRECT_PORT", port.to_string());
    let (launched, _proxy) = fixture
        .launch_with_env(
            &fake,
            &fixture.browser_cache().join("profiles"),
            Duration::from_secs(20),
            SandboxConfig::default(),
            &env,
        )
        .await
        .unwrap();
    let probe = std::fs::read_to_string(launched.profile_dir.join("probe")).unwrap();
    assert_eq!(
        probe.lines().collect::<Vec<_>>(),
        ["refused", "blocked"],
        "{probe}"
    );
    assert!(!outside.join("escaped").exists());
    assert!(
        service.accept().is_err(),
        "the service was reached directly"
    );
    assert!(launched.profile_dir.join("written").exists());
    assert!(launched.temp_dir.join("written").exists());
    launched.terminate().await;
}

#[tokio::test]
async fn no_consent() {
    // GIVEN no `browser.real_profile` grant WHEN a browser session is
    // created THEN it launches with a throwaway profile.
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let session = manager.session("ses_b/default").await.unwrap();
    assert!(session.pid().is_some(), "a local browser was launched");
    let cache = fixture.browser_cache();
    let records = support::records(&cache);
    assert_eq!(records.len(), 1, "{records:?}");
    let record: Record = serde_json::from_slice(&std::fs::read(&records[0]).unwrap()).unwrap();
    let profile = record.profile_dir;
    assert!(
        profile.starts_with(cache.join("profiles")),
        "{} is not a throwaway profile",
        profile.display()
    );
    assert!(profile.exists());
    manager.shutdown().await;
    assert!(!profile.exists(), "the throwaway profile is removed");
}
