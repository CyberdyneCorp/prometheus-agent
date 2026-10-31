//! `prometheus doctor` (spec: workspace-architecture, Doctor; Doctor
//! repairs are explicit and enumerated; cli, Doctor; security-boundaries,
//! doctor security report).

mod common;

use std::net::TcpListener;

use common::{mock_home, ok, run, stderr, stdout, temp_home};

/// Scenario: Missing secret reported.
#[test]
fn a_missing_provider_secret_fails_with_its_remedy() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "anthropic"],
    ));
    let out = run(home.path(), &["doctor"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    let line = text
        .lines()
        .find(|l| l.starts_with("fail") && l.contains("ANTHROPIC_API_KEY"))
        .unwrap_or_else(|| panic!("no fail line:\n{text}"));
    assert!(
        line.contains("prometheus secrets set ANTHROPIC_API_KEY"),
        "{line}"
    );
}

#[test]
fn a_healthy_profile_passes_and_json_has_its_shape() {
    let home = mock_home();
    let out = run(home.path(), &["doctor", "store"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let doc: serde_json::Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "doctor", "config"]))).unwrap();
    assert_eq!(doc["schema"], "prometheus.doctor.v1");
    assert_eq!(doc["failed"], false);
    assert!(
        doc["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["group"] == "config")
    );
    let out = run(home.path(), &["doctor", "nosuch"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("groups: home, config"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn doctor_security_lists_every_sandbox_feature() {
    let home = mock_home();
    let text = ok(&run(home.path(), &["doctor", "security"]));
    for feature in ["fs: ", "net: ", "process: ", "syscalls: "] {
        assert!(text.contains(feature), "{feature} missing:\n{text}");
    }
    assert!(text.contains("boundary: OS sandbox"), "{text}");
}

/// Scenario: Offline doctor (and `--online` does connect).
#[test]
fn no_connection_is_made_without_online() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "ollama"],
    ));
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "providers.ollama.base_url",
            &format!("http://127.0.0.1:{port}/v1"),
        ],
    ));
    let offline = run(home.path(), &["doctor"]);
    assert!(
        stdout(&offline).contains("not probed (use --online)"),
        "{}",
        stdout(&offline)
    );
    assert!(
        listener.accept().is_err(),
        "doctor connected without --online"
    );
    let online = run(home.path(), &["doctor", "providers", "--online"]);
    assert!(
        stdout(&online).contains(&format!("127.0.0.1:{port} reachable")),
        "{}",
        stdout(&online)
    );
    assert!(listener.accept().is_ok(), "--online probes the provider");
}

/// A stale full-text index is reported, rebuilt by `--fix` and printed.
#[test]
fn fix_rebuilds_a_stale_full_text_index() {
    let home = mock_home();
    ok(&run(home.path(), &["-z", "index me"]));
    let db = home.path().join("profiles/default/state.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES ('index_stale', '1') \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [],
    )
    .unwrap();
    drop(conn);
    let out = run(home.path(), &["doctor", "store"]);
    assert_ne!(
        stdout(&out).lines().filter(|l| l.starts_with("ok")).count(),
        0
    );
    assert!(
        stdout(&out)
            .lines()
            .any(|l| !l.starts_with("ok") && l.contains("store")),
        "{}",
        stdout(&out)
    );
    let fixed = ok(&run(home.path(), &["doctor", "store", "--fix"]));
    assert!(fixed.lines().any(|l| l.starts_with("fixed ")), "{fixed}");
    let again = ok(&run(home.path(), &["doctor", "store", "--fix"]));
    assert!(!again.contains("fixed "), "{again}");
}

#[cfg(unix)]
mod repairs {
    use std::os::unix::fs::PermissionsExt as _;

    use super::common::{mock_home, ok, run, stdout};

    /// Scenario: World-readable secrets store; `--fix` tightens it.
    #[test]
    fn a_world_readable_secrets_store_fails_until_fixed() {
        let home = mock_home();
        let secrets = home.path().join("profiles/default/secrets.toml");
        std::fs::write(&secrets, "").unwrap();
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = run(home.path(), &["doctor", "secrets"]);
        assert_eq!(out.status.code(), Some(1));
        let line = stdout(&out)
            .lines()
            .find(|l| l.starts_with("fail") && l.contains("secrets.toml"))
            .map(str::to_owned)
            .unwrap_or_default();
        assert!(line.contains("prometheus doctor --fix"), "{}", stdout(&out));
        // Opening the home tightens owner files on every start (with a
        // WARN), so make it loose again for the repair.
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
        let fixed = ok(&run(home.path(), &["doctor", "secrets", "--fix"]));
        assert!(
            fixed.contains("fixed tightened") && fixed.contains("secrets.toml from 644 to 600"),
            "{fixed}"
        );
        let mode = std::fs::metadata(&secrets).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Scenario: Stale lock repaired.
    #[test]
    fn a_stale_gateway_lock_is_removed_and_printed() {
        let home = mock_home();
        let run_dir = home.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("gateway.lock"), "").unwrap();
        std::fs::write(
            run_dir.join("gateway.json"),
            r#"{"pid":999999,"start_time":1,"version":"0.0.1","role":"daemon","profiles":["default"]}"#,
        )
        .unwrap();
        let out = ok(&run(home.path(), &["doctor", "daemon", "--fix"]));
        assert!(
            out.contains("removed stale gateway lock (pid 999999 not running)"),
            "{out}"
        );
        assert!(!run_dir.join("gateway.json").exists());
        assert!(!run_dir.join("gateway.lock").exists());
        let again = ok(&run(home.path(), &["doctor", "daemon", "--fix"]));
        assert!(
            !again.contains("removed"),
            "repairs are printed only when performed: {again}"
        );
    }
}
