//! The embedded `google_meet` plugin (spec: plugin-system, First-party
//! `google_meet` plugin), hosted by the python bridge for real. The browser
//! runtime is a stand-in: an executable at the runtime interpreter's path
//! that records its start and waits like the meeting bot, so the tests
//! need no Chromium or Google account. Needs uv on PATH (see
//! `pmt_pybridge::testkit`).
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, ok, stderr, stdout};

fn data(home: &Path) -> PathBuf {
    home.join("profiles/default/plugin-data/google_meet")
}

fn meetings(home: &Path) -> PathBuf {
    home.join("profiles/default/workspace/meetings")
}

fn enable(home: &Path) {
    pmt_pybridge::testkit::seed(home);
    ok(&command(home)
        .args(["plugins", "enable", "google_meet", "--grant", "all"])
        .stdin(Stdio::null())
        .output()
        .unwrap());
}

/// A browser runtime whose "interpreter" plays the bot: it writes its
/// arguments and a joined `status.json` to the meeting directory, then
/// waits until it is stopped. With `signed_in`, the saved sign-in too.
fn fake_runtime(home: &Path, signed_in: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let runtime = data(home).join("runtime");
    std::fs::create_dir_all(runtime.join("bin")).unwrap();
    let python = runtime.join("bin/python");
    std::fs::write(
        &python,
        "#!/bin/sh\n\
         echo \"$PROMETHEUS_MEET_URL\" > \"$PROMETHEUS_MEET_OUT_DIR/bot.started\"\n\
         printf '{\"inCall\": true, \"lobbyWaiting\": false, \"transcriptLines\": 0}' > \"$PROMETHEUS_MEET_OUT_DIR/status.json\"\n\
         exec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(runtime.join(".ready"), "{}").unwrap();
    if signed_in {
        std::fs::write(data(home).join("auth.json"), "{\"cookies\": []}").unwrap();
    }
}

/// One turn driven by the mock `script`.
fn turn(home: &Path, script: &str) -> std::process::Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(home)
        .args(["-z", "go", "--mock-script"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn tool_results(home: &Path) -> Vec<String> {
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The JSON a plugin tool answered (inside its untrusted-content frame).
fn json(result: &str) -> serde_json::Value {
    let start = result.find('{').unwrap_or_else(|| panic!("{result}"));
    let end = result.rfind('}').unwrap_or_else(|| panic!("{result}"));
    serde_json::from_str(&result[start..=end]).unwrap_or_else(|e| panic!("{e}: {result}"))
}

const OFFERED: &str = r#"
[[rule]]
tools = "meet_join"
text = "offered"

[[rule]]
text = "absent"
"#;

#[test]
fn the_tools_are_offered_only_with_the_runtime_and_the_sign_in() {
    let home = mock_home();
    enable(home.path());
    let out = turn(home.path(), OFFERED);
    assert_eq!(stdout(&out), "absent\n", "{}", stderr(&out));
    fake_runtime(home.path(), false);
    let out = turn(home.path(), OFFERED);
    assert_eq!(stdout(&out), "absent\n", "no sign-in: {}", stderr(&out));
    fake_runtime(home.path(), true);
    let out = turn(home.path(), OFFERED);
    assert_eq!(stdout(&out), "offered\n", "{}", stderr(&out));
}

#[test]
fn non_meet_url_rejected() {
    let home = mock_home();
    enable(home.path());
    fake_runtime(home.path(), true);
    let out = turn(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "meet_join"
arguments = { url = "https://zoom.us/j/123" }

[[rule]]
tool_result = "only meet.google.com URLs are accepted"
text = "refused"
"#,
    );
    assert_eq!(stdout(&out), "refused\n", "{}", stderr(&out));
    assert!(
        !meetings(home.path()).exists() || common::files_under(&meetings(home.path())).is_empty(),
        "no browser started"
    );
    // The CLI refuses it the same way.
    let out = command(home.path())
        .args([
            "plugins",
            "run",
            "google_meet",
            "join",
            "https://meet.google.com.evil.example/abc-defg-hij",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("only meet.google.com URLs are accepted"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_join_runs_in_the_background_and_leave_stops_it() {
    let home = mock_home();
    enable(home.path());
    fake_runtime(home.path(), true);
    let out = turn(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "meet_join"
arguments = { url = "https://meet.google.com/abc-defg-hij" }

[[rule]]
tool_result = "transcriptPath"
text = "left"

[[rule]]
tool_result = "startedAt"
[[rule.tool_calls]]
name = "meet_leave"
arguments = {}

[[rule]]
tool_result = "log_path"
# The bot starts in the background: give it a moment before asking.
latency_ms = 1500
[[rule.tool_calls]]
name = "meet_status"
arguments = {}
"#,
    );
    assert_eq!(stdout(&out), "left\n", "{}", stderr(&out));
    let results = tool_results(home.path());
    let joined = json(&results[0]);
    assert_eq!(joined["success"], true, "{joined}");
    assert_eq!(joined["meeting_id"], "abc-defg-hij", "{joined}");
    let status: serde_json::Value = json(&results[1]);
    assert_eq!(status["alive"], true, "{status}");
    assert_eq!(status["inCall"], true, "{status}");
    let left: serde_json::Value = json(&results[2]);
    assert_eq!(left["success"], true, "{left}");
    // Artifacts live in the profile's workspace; the bot is gone.
    let dir = meetings(home.path()).join("abc-defg-hij");
    let log = std::fs::read_to_string(dir.join("bot.log")).unwrap_or_default();
    assert_eq!(
        std::fs::read_to_string(dir.join("bot.started"))
            .unwrap_or_else(|e| panic!("{e}: {log}"))
            .trim(),
        "https://meet.google.com/abc-defg-hij"
    );
    assert!(dir.join("leave.request").is_file());
    assert!(!meetings(home.path()).join(".active.json").exists());
    let pid = joined["pid"].as_u64().unwrap();
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the bot process {pid} was stopped");
}

#[test]
fn setup_reports_what_is_missing() {
    let home = mock_home();
    enable(home.path());
    let out = command(home.path())
        .args(["plugins", "run", "google_meet", "setup"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("browser runtime: not installed"), "{text}");
    assert!(text.contains("google sign-in : not saved"), "{text}");
}
