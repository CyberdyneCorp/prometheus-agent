//! Supervisor units, the daemon inventory across homes and relay
//! enrollment (spec: messaging-gateway, Per-OS service installation
//! without elevation; Daemon inventory across homes; Relay enrollment).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pmt_rpc::discovery::{Discovery, discover};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use self::common::{command, ok, run, stderr, stdout, temp_home};

/// The binary on `home` whose OS user home is `user` (shared by homes).
fn as_user(home: &Path, user: &Path) -> Command {
    let mut cmd = command(home);
    cmd.env("HOME", user).env("USERPROFILE", user);
    if cfg!(windows) {
        cmd.env("LOCALAPPDATA", user.join("AppData/Local"));
    }
    cmd
}

struct Foreground(Child);

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(home: &Path, user: &Path) -> Foreground {
    let child = as_user(home, user)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(discover(home), Ok(Discovery::Running(_))) {
        assert!(
            Instant::now() < deadline,
            "the daemon of {} did not start",
            home.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Foreground(child)
}

fn list(home: &Path, user: &Path) -> Value {
    let out = as_user(home, user)
        .args(["--json", "gateway", "list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    serde_json::from_str(&stdout(&out)).unwrap()
}

#[test]
fn two_homes_on_one_machine_are_both_listed() {
    let user = temp_home();
    let (a, b) = (temp_home(), temp_home());
    let empty = list(a.path(), user.path());
    assert_eq!(
        empty["daemons"],
        serde_json::json!([]),
        "no daemon: an empty list"
    );
    let first = start(a.path(), user.path());
    let second = start(b.path(), user.path());
    let listed = list(a.path(), user.path());
    let daemons = listed["daemons"].as_array().unwrap();
    assert_eq!(daemons.len(), 2, "{listed}");
    for (home, child) in [(a.path(), &first), (b.path(), &second)] {
        let entry = daemons
            .iter()
            .find(|d| Path::new(d["home"].as_str().unwrap()) == home)
            .unwrap_or_else(|| panic!("{} missing from {listed}", home.display()));
        assert_eq!(entry["pid"], child.0.id());
        assert_eq!(entry["profiles"], serde_json::json!(["default"]));
        assert_eq!(entry["state"], "running");
    }
    ok(&as_user(a.path(), user.path())
        .args(["daemon", "stop"])
        .output()
        .unwrap());
    drop(first);
    let after = list(b.path(), user.path());
    assert_eq!(after["daemons"].as_array().unwrap().len(), 1, "{after}");
}

/// `daemon install-service` against the recording seam.
fn service(home: &Path, seam: &Path, args: &[&str]) -> std::process::Output {
    let mut all = vec!["daemon"];
    all.extend(args);
    command(home)
        .args(&all)
        .env("PMT_TEST_SERVICE_DIR", seam)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn commands(seam: &Path) -> String {
    std::fs::read_to_string(seam.join("commands.log")).unwrap_or_default()
}

#[test]
fn system_scope_needs_root() {
    let home = temp_home();
    let seam = temp_home();
    let out = service(
        home.path(),
        seam.path(),
        &["install-service", "--system", "--run-as-user", "alice"],
    );
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(stderr(&out).contains("system service install requires root"));
    assert_eq!(commands(seam.path()), "", "nothing was installed");
}

#[cfg(unix)]
#[test]
fn doctor_reports_legacy_units_with_the_command_that_replaces_them() {
    let home = temp_home();
    let seam = temp_home();
    let user_unit = seam
        .path()
        .join("home/.config/systemd/user/hermes-gateway-work.service");
    let system_unit = seam
        .path()
        .join("system/etc/systemd/system/hermes-gateway.service");
    for (unit, text) in [
        (&user_unit, "[Unit]\n"),
        (&system_unit, "[Service]\nUser=alice\n"),
    ] {
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(unit, text).unwrap();
    }
    let out = command(home.path())
        .args(["doctor", "daemon"])
        .env("PMT_TEST_SERVICE_DIR", seam.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = stdout(&out);
    let install = format!(
        "prometheus --home '{}' daemon install-service",
        home.path().display()
    );
    assert!(
        text.contains(&format!(
            "systemctl --user disable --now hermes-gateway-work.service && rm '{}' && \
             systemctl --user daemon-reload && {install}",
            user_unit.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "sudo systemctl disable --now hermes-gateway.service && sudo rm '{}' && \
             sudo systemctl daemon-reload && sudo {install} --system --run-as-user 'alice'",
            system_unit.display()
        )),
        "{text}"
    );
    assert!(user_unit.exists() && system_unit.exists(), "never removed");
}

/// `prometheus doctor daemon` against the recording seam.
#[cfg(windows)]
fn doctor(home: &Path, seam: &Path) -> String {
    let out = command(home)
        .args(["doctor", "daemon"])
        .env("PMT_TEST_SERVICE_DIR", seam)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    stdout(&out)
}

#[cfg(windows)]
#[test]
fn doctor_reports_hermes_windows_launchers_with_the_command_that_replaces_them() {
    let home = temp_home();
    let seam = temp_home();
    std::fs::write(
        seam.path().join("output-schtasks"),
        "\"\\Hermes_Gateway\",\"N/A\",\"Ready\"\r\n\"\\Prometheus Daemon\",\"N/A\",\"Ready\"\r\n",
    )
    .unwrap();
    let startup = seam
        .path()
        .join(r"home\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup");
    std::fs::create_dir_all(&startup).unwrap();
    let launcher = startup.join("Hermes_Gateway_work.vbs");
    std::fs::write(&launcher, "").unwrap();
    let text = doctor(home.path(), seam.path());
    let install = format!(
        "prometheus --home \"{}\" daemon install-service",
        home.path().display()
    );
    assert!(
        text.contains(&format!(
            "schtasks /End /TN \"Hermes_Gateway\" & schtasks /Delete /TN \"Hermes_Gateway\" /F && {install}"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("del \"{}\" && {install}", launcher.display())),
        "{text}"
    );
    assert!(!text.contains("Prometheus Daemon"), "{text}");
    assert!(launcher.exists(), "never removed");
    let log = commands(seam.path());
    assert!(!log.contains("/Delete"), "nothing removed: {log}");
}

#[cfg(target_os = "macos")]
#[test]
fn reinstalling_the_launch_agent_rewrites_one_plist() {
    let home = temp_home();
    let seam = temp_home();
    let plist = seam
        .path()
        .join("home/Library/LaunchAgents/ai.prometheus.daemon.plist");
    for _ in 0..2 {
        let out = service(home.path(), seam.path(), &["install-service"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }
    let agents: Vec<_> = std::fs::read_dir(plist.parent().unwrap())
        .unwrap()
        .collect();
    assert_eq!(agents.len(), 1, "exactly one LaunchAgent");
    let text = std::fs::read_to_string(&plist).unwrap();
    assert!(text.contains("<string>ai.prometheus.daemon</string>"));
    assert!(
        text.contains(&home.path().to_string_lossy().into_owned()),
        "the home is named"
    );
    let log = commands(seam.path());
    // Each install unloads the previous copy before loading, so one stays loaded.
    assert_eq!(log.matches("launchctl bootout gui/").count(), 2, "{log}");
    assert_eq!(log.matches("launchctl bootstrap gui/").count(), 2, "{log}");
    let out = service(home.path(), seam.path(), &["uninstall-service"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!plist.exists());
    let again = service(home.path(), seam.path(), &["uninstall-service"]);
    assert_eq!(again.status.code(), Some(0));
    assert!(stdout(&again).contains("no service installed"));
}

#[cfg(target_os = "linux")]
#[test]
fn reinstalling_the_user_unit_rewrites_one_unit() {
    let home = temp_home();
    let seam = temp_home();
    let unit = seam
        .path()
        .join("home/.config/systemd/user/prometheus-daemon.service");
    for _ in 0..2 {
        let out = service(home.path(), seam.path(), &["install-service", "--no-start"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }
    let units: Vec<_> = std::fs::read_dir(unit.parent().unwrap()).unwrap().collect();
    assert_eq!(units.len(), 1);
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(text.contains("RestartPreventExitStatus=78"));
    let log = commands(seam.path());
    assert!(
        log.contains("systemctl --user enable prometheus-daemon.service"),
        "{log}"
    );
    assert!(!log.contains("--now"), "--no-start: {log}");
    assert!(log.contains("loginctl enable-linger"), "{log}");
    std::fs::write(seam.path().join("root"), "").unwrap();
    let out = service(
        home.path(),
        seam.path(),
        &["install-service", "--system", "--run-as-user", "alice"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let system = std::fs::read_to_string(
        seam.path()
            .join("system/etc/systemd/system/prometheus-daemon.service"),
    )
    .unwrap();
    assert!(system.contains("User=alice"));
}

#[cfg(windows)]
#[test]
fn without_task_scheduler_rights_a_startup_launcher_is_used() {
    let home = temp_home();
    let seam = temp_home();
    std::fs::write(seam.path().join("refuse-schtasks"), "").unwrap();
    let out = service(home.path(), seam.path(), &["install-service"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("used the fallback"),
        "{}",
        stdout(&out)
    );
    let launcher = seam.path().join(
        r"home\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\Prometheus Daemon.cmd",
    );
    let text = std::fs::read_to_string(launcher).unwrap();
    assert!(text.contains("daemon start"), "{text}");
    let log = commands(seam.path());
    assert!(log.contains("/SC ONLOGON"), "{log}");
    assert!(log.contains("/RL LIMITED"), "{log}");
}

fn daemon_toml(home: &Path) -> String {
    std::fs::read_to_string(home.join("daemon.toml")).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reused_enrollment_token_changes_nothing() {
    let connector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/enroll"))
        .respond_with(ResponseTemplate::new(409).set_body_string(r#"{"error":"already_used"}"#))
        .mount(&connector)
        .await;
    let home = temp_home();
    let before = daemon_toml(home.path());
    let url = connector.uri();
    let home_path = home.path().to_path_buf();
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &["gateway", "enroll", "--token", "T", "--connector-url", &url],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("enrollment token rejected: already used"));
    assert_eq!(daemon_toml(home.path()), before);
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(!secrets.contains("PMT_RELAY_TOKEN"), "{secrets}");
}

#[tokio::test(flavor = "multi_thread")]
async fn enrollment_stores_the_credential_and_writes_the_relay() {
    let connector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/enroll"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"gateway_id":"gw-7","credential":"long-lived-secret-value"}"#),
        )
        .mount(&connector)
        .await;
    let home = temp_home();
    let url = connector.uri();
    let home_path = home.path().to_path_buf();
    let out = pmt_core::spawn::blocking(move || {
        run(
            &home_path,
            &[
                "gateway",
                "enroll",
                "--token",
                "one-time-tok",
                "--connector-url",
                &url,
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let printed = format!("{}{}", stdout(&out), stderr(&out));
    assert!(!printed.contains("long-lived-secret-value"), "{printed}");
    assert!(!printed.contains("one-time-tok"), "{printed}");
    let config = daemon_toml(home.path());
    assert!(config.contains("gateway_id = \"gw-7\""), "{config}");
    let ws = connector.uri().replace("http://", "ws://");
    assert!(
        config.contains(&format!("url = \"{ws}/v1/gateway\"")),
        "{config}"
    );
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(secrets.contains("PMT_RELAY_TOKEN"), "{secrets}");
    assert!(!secrets.contains("long-lived-secret-value"));
}
