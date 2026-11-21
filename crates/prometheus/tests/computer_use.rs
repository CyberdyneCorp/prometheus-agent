//! Computer use end to end against a stand-in `cua-driver` (spec:
//! computer-use): the tool driving a desktop under approval, the driver's
//! runtimes and the machine they act on, its availability, and the
//! `prometheus computer-use` commands.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{append_config, command, mock_home, ok, stderr, stdout};
use serde_json::Value;

/// A home with a stand-in driver at `computer_use.driver_path`.
struct Fixture {
    home: common::TestHome,
    /// The stand-in's directory (`fake.json`, `log.jsonl`).
    bin: PathBuf,
}

impl Fixture {
    fn new(settings: &str, config: &str) -> Self {
        let home = mock_home();
        let bin = home.path().join("driver-bin");
        Self::install_fake(&bin, settings);
        append_config(
            home.path(),
            &format!(
                "\n[computer_use]\ndriver_path = {:?}\n{config}\n",
                bin.join("cua-driver").display().to_string()
            ),
        );
        Self { home, bin }
    }

    /// The stand-in driver copied into `dir` with `settings`.
    fn install_fake(dir: &Path, settings: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let fake =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/computer-use/cua-driver");
        let exe = dir.join("cua-driver");
        std::fs::copy(fake, &exe).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("fake.json"), settings).unwrap();
    }

    /// `PATH` with `extra` first.
    fn path(extra: &[PathBuf]) -> OsString {
        let mut dirs = extra.to_vec();
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(dirs).unwrap()
    }

    /// A one-shot run of `script` with only the `computer_use` toolset, on
    /// an X11-like desktop.
    fn chat(&self, script: &str, extra: &[&str], path: &OsString) -> Value {
        let out = self.chat_output(script, extra, path);
        serde_json::from_str(stdout(&out).trim())
            .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(&out), stderr(&out)))
    }

    /// [`Fixture::chat`]'s raw output (a run that ends on a refusal).
    fn chat_output(&self, script: &str, extra: &[&str], path: &OsString) -> Output {
        let file = self.home.path().join("script.toml");
        std::fs::write(&file, script).unwrap();
        command(self.home.path())
            .current_dir(self.home.path())
            .env("PATH", path)
            .env("DISPLAY", ":42")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
            .env("XDG_RUNTIME_DIR", self.home.path().join("fake-state"))
            .env("OPENAI_API_KEY", "sk-never-for-the-driver")
            .args(["--json", "-z", "drive", "--toolsets", "computer_use"])
            .args(extra)
            .arg("--mock-script")
            .arg(&file)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.bin.join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn calls(&self, name: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["call"] == name)
            .collect()
    }

    fn tool_results(&self) -> Vec<String> {
        let session = common::newest_session(self.home.path());
        common::session_messages(self.home.path(), &session)
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["content"].as_str().map(str::to_owned))
            .collect()
    }

    fn cli(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = command(self.home.path());
        cmd.args(args)
            .stdin(Stdio::null())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }
}

const DRIVE: &str = r#"
[[rule]]
user = "drive"
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "capture", mode = "som" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "click", element = 2 }

[[rule]]
step = 2
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "type", text = "hello" }

[[rule]]
repeat = true
text = "done"
"#;

/// The tool drives the stand-in desktop: captures need no approval, input
/// does (`single_query_mode = "approve"` here), the driver sees its own
/// sanitized environment with telemetry off, and the capture is kept.
#[test]
fn capture_click_and_type_drive_the_desktop_under_approval() {
    let f = Fixture::new("{}", "\n[approvals]\nsingle_query_mode = \"approve\"\n");
    let session = f.chat(DRIVE, &[], &Fixture::path(&[]));
    assert_eq!(session["answer"], "done", "{session}");
    let results = f.tool_results();
    assert!(results[0].contains("on display :42"), "{}", results[0]);
    let capture: Value = serde_json::from_str(&results[0]).unwrap();
    let shot = capture["screenshot_path"].as_str().unwrap();
    assert!(shot.contains("cache/computer-use/capture-"), "{shot}");
    assert!(Path::new(shot).exists());
    let clicks = f.calls("click");
    assert_eq!(clicks.len(), 1, "{:?}", f.log());
    assert_eq!(clicks[0]["arguments"]["element_index"], 2);
    assert_eq!(clicks[0]["arguments"]["pid"], 11);
    assert_eq!(f.calls("type_text")[0]["arguments"]["text"], "hello");
    let launched = f.log().into_iter().find(|e| e["argv"][0] == "mcp").unwrap();
    assert_eq!(launched["env"]["CUA_DRIVER_RS_TELEMETRY_ENABLED"], "0");
    assert_eq!(
        launched["env"]["OPENAI_API_KEY"],
        Value::Null,
        "no secrets reach the driver"
    );
    // Scenario: X11 desktop — the overlay starts disabled (macOS: always).
    assert!(
        launched["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--no-overlay"),
        "{launched}"
    );
    let cursor = f.calls("set_agent_cursor_enabled");
    assert_eq!(cursor[0]["arguments"]["enabled"], false);
}

/// Scenario: Cron run (single-query flavour): with the default deny, the
/// capture runs and the click is refused before anything reaches the
/// desktop.
#[test]
fn unattended_runs_refuse_input_by_default() {
    let f = Fixture::new("{}", "");
    let out = f.chat_output(DRIVE, &[], &Fixture::path(&[]));
    assert!(
        stderr(&out).contains("chat.approval_required"),
        "{}",
        stderr(&out)
    );
    assert!(
        !f.calls("get_window_state").is_empty(),
        "captures need no approval"
    );
    assert!(f.calls("click").is_empty() && f.calls("type_text").is_empty());
}

/// Under `/yolo` the session launches a private unrestricted runtime of
/// its own, stopped when the session ends (Linux: macOS launches private
/// runtimes only through the signed bundle).
#[cfg(target_os = "linux")]
#[test]
fn yolo_launches_a_private_unrestricted_runtime() {
    let f = Fixture::new("{}", "");
    f.chat(DRIVE, &["--yolo"], &Fixture::path(&[]));
    assert_eq!(f.calls("type_text").len(), 1);
    let log = f.log();
    let serve = log
        .iter()
        .find(|e| e["argv"][0] == "serve")
        .expect("a private daemon");
    let argv: Vec<&str> = serve["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--permission-mode", "unrestricted"]),
        "{argv:?}"
    );
    assert!(argv.contains(&"--dangerously-bypass-approvals"));
    assert!(
        log.iter().any(|e| e["argv"][0] == "stop"),
        "the daemon stops with the session"
    );
}

/// Scenario: Unsigned bundle.
#[cfg(target_os = "macos")]
#[test]
fn a_bounded_runtime_refuses_an_unsigned_bundle() {
    let home = mock_home();
    let bin = home.path().join("Local/CuaDriver.app/Contents/MacOS");
    Fixture::install_fake(&bin, "{}");
    let manifest = home.path().join("manifest.yaml");
    std::fs::write(&manifest, "version: 3\n").unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[computer_use]\ndriver_path = {:?}\npermission_mode = \"bounded\"\ncapability_manifest = {:?}\n",
            bin.join("cua-driver").display().to_string(),
            manifest.display().to_string()
        ),
    );
    let f = Fixture { home, bin };
    f.chat_output(DRIVE, &[], &Fixture::path(&[]));
    let results = f.tool_results();
    assert!(
        results[0].contains("code signature check refused"),
        "{}",
        results[0]
    );
    assert!(
        f.log().iter().all(|e| e["argv"][0] != "serve"),
        "nothing launched"
    );
}

/// Scenario: Container backend.
#[test]
fn a_container_session_captures_the_containers_display() {
    let docker =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot-screen/backends/docker");
    let f = Fixture::new(
        "{}",
        &format!(
            "\n[approvals]\nsingle_query_mode = \"approve\"\n[terminal]\nbackend = \"container\"\n[terminal.container]\nruntime = {:?}\n[bot_desktop]\nmin_free_memory_bytes = 0\n",
            docker.display().to_string()
        ),
    );
    let fakes = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot-screen");
    let path = Fixture::path(&[f.bin.clone(), fakes.join("backends"), fakes]);
    let state = f.home.path().join("fake-state");
    std::fs::create_dir_all(&state).unwrap();
    let screen = |args: &[&str]| {
        command(f.home.path())
            .args(["computer-use", "screen"])
            .args(args)
            .env("PATH", &path)
            .env("XDG_RUNTIME_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    ok(&screen(&["start"]));
    f.chat(DRIVE, &[], &path);
    let _ = screen(&["stop", "--force"]);
    let results = f.tool_results();
    assert!(
        results[0].contains("on display :1"),
        "the container's display: {}",
        results[0]
    );
    let docker_log = std::fs::read_to_string(state.join("fake-docker.log")).unwrap();
    let exec = docker_log
        .lines()
        .find(|line| line.contains("cua-driver"))
        .expect("the driver ran in the desktop container");
    assert!(
        exec.contains("\"exec\"") && exec.contains("pmt-screen-"),
        "{exec}"
    );
}

/// Scenario: Driver missing.
#[test]
fn without_a_driver_and_lazy_installs_the_tool_is_unavailable() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[runtimes]\nallow_lazy_install = false\n",
    )
    .unwrap();
    let out = command(home.path())
        .args(["--json", "tools", "list"])
        .env("DISPLAY", ":42")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    let row = doc["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "computer_use")
        .unwrap()
        .clone();
    assert_eq!(
        row["exposure"], "unavailable:cua-driver is not installed and lazy installs are disabled",
        "{row}"
    );
}

/// Scenario: Unrestricted via config.
#[test]
fn unrestricted_is_not_a_config_value() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[computer_use]\npermission_mode = \"unrestricted\"\n",
    );
    let out = common::run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
}

/// Scenario: Driver not installed.
#[test]
fn the_doctor_without_a_driver_exits_69_with_the_install_hint() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[runtimes]\nallow_lazy_install = false\n",
    )
    .unwrap();
    let out = common::run(home.path(), &["computer-use", "doctor"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("lazy installs are disabled")
            && stderr(&out).contains("prometheus computer-use install"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_doctor_prints_the_check_matrix() {
    let f = Fixture::new("{}", "");
    let out = f.cli(
        &["--json", "computer-use", "doctor", "--skip", "daemon"],
        &[],
    );
    let doc: Value =
        serde_json::from_str(&stdout(&out)).unwrap_or_else(|e| panic!("{e}: {}", stderr(&out)));
    let checks: Vec<&str> = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["check"].as_str())
        .collect();
    assert_eq!(
        checks,
        [
            "binary_version",
            "platform",
            "session",
            "bundle_identity",
            "accessibility",
            "screen_recording",
            "capture",
            "telemetry"
        ]
    );
    let status = |name: &str| {
        doc["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == name)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(status("capture"), "pass");
    assert_eq!(status("telemetry"), "pass");
    if cfg!(target_os = "macos") {
        // The stand-in is not inside a signed CuaDriver.app.
        assert_eq!(status("bundle_identity"), "fail");
        assert_eq!(out.status.code(), Some(1));
    } else {
        assert_eq!(status("bundle_identity"), "skip");
        assert_eq!(out.status.code(), Some(0), "{doc}");
    }
}

/// Scenario: Missing Screen Recording (macOS); a missing display on
/// Linux.
#[test]
fn permissions_status_lists_each_grant_and_fails_on_a_missing_one() {
    let f = Fixture::new(r#"{"screen_recording": false}"#, "");
    let out = f.cli(&["computer-use", "permissions", "status"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    if cfg!(target_os = "macos") {
        assert!(text.contains("accessibility: granted"), "{text}");
        assert!(text.contains("screen_recording: missing"), "{text}");
    } else {
        assert!(text.contains("display: missing"), "{text}");
    }
}

#[test]
fn status_names_the_driver_and_the_mode() {
    let f = Fixture::new("{}", "");
    let out = f.cli(&["--json", "computer-use", "status"], &[]);
    let doc: Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["backend"], "cua");
    assert_eq!(doc["version"], "0.21.0");
    assert_eq!(doc["source"], "driver_path");
    assert_eq!(doc["permission_mode"], "standard");
}

/// A bounded profile whose manifest is missing fails every session at
/// start with the path, whatever toolsets the session selects: it never
/// runs unbounded.
#[test]
fn a_bounded_session_without_its_manifest_fails_at_start_with_the_path() {
    let home = mock_home();
    let manifest = home.path().join("reviewed/manifest.yaml");
    append_config(
        home.path(),
        &format!(
            "\n[computer_use]\npermission_mode = \"bounded\"\ncapability_manifest = {:?}\n",
            manifest.display().to_string()
        ),
    );
    let f = Fixture {
        bin: home.path().join("driver-bin"),
        home,
    };
    for toolsets in [
        &["--toolsets", "computer_use"][..],
        &["--toolsets", "file"][..],
    ] {
        let file = f.home.path().join("script.toml");
        std::fs::write(&file, DRIVE).unwrap();
        let out = command(f.home.path())
            .current_dir(f.home.path())
            .args(["--json", "-z", "drive"])
            .args(toolsets)
            .arg("--mock-script")
            .arg(&file)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("computer_use.manifest"), "{err}");
        assert!(err.contains(&manifest.display().to_string()), "{err}");
    }
    assert!(f.log().is_empty(), "no driver ran");
}

/// `computer_use.driver_path` is never installed over or replaced.
#[test]
fn install_leaves_a_configured_driver_alone() {
    let f = Fixture::new("{}", "");
    for args in [
        &["computer-use", "install"][..],
        &["computer-use", "install", "--upgrade"][..],
    ] {
        let out = f.cli(args, &[]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!(
                "computer_use.driver_path is set: {} is used as it is (never installed or replaced)",
                f.bin.join("cua-driver").display()
            )),
            "{}",
            stderr(&out)
        );
    }
    assert!(
        !f.home.path().join("runtimes/cua-driver").exists(),
        "nothing installed"
    );
}

/// `permissions grant` opens the settings pane of each missing grant on
/// macOS (through `open`, a stand-in here).
#[cfg(target_os = "macos")]
#[test]
fn permissions_grant_opens_the_pane_of_each_missing_grant() {
    let f = Fixture::new(r#"{"screen_recording": false}"#, "");
    let stand_in = f.home.path().join("open-bin");
    std::fs::create_dir_all(&stand_in).unwrap();
    let opened = stand_in.join("opened.log");
    let open = stand_in.join("open");
    std::fs::write(
        &open,
        format!(
            "#!/bin/sh\necho \"$@\" >> {:?}\n",
            opened.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = Fixture::path(&[stand_in]);
    let out = f.cli(
        &["computer-use", "permissions", "grant"],
        &[("PATH", path.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("grant it to CuaDriver (com.trycua.driver)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(&opened).unwrap(),
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture\n",
        "only the missing grant's pane"
    );
}

/// Elsewhere the prerequisites have no settings pane to open.
#[cfg(not(target_os = "macos"))]
#[test]
fn permissions_grant_has_nothing_to_open_off_macos() {
    let f = Fixture::new("{}", "");
    let out = f.cli(&["computer-use", "permissions", "grant"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing to grant here"),
        "{}",
        stderr(&out)
    );
}
