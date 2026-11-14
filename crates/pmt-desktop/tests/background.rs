//! The shell's background, quick-ask and install requests (spec:
//! desktop-app, Approval notifications never approve; Global hotkey
//! quick-ask; Install deep links open a consent flow; Desktop launch
//! options), against a recording machine integration and a fake bundled
//! binary that logs every run.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_desktop::features::Hotkey;
use pmt_desktop::{Native, Request, Shell, ShellConfig, options};
use serde_json::json;

/// Records the OS calls; `taken` makes hotkey registration fail.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    taken: bool,
}

impl Recorder {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, call: String) -> Result<(), String> {
        self.calls.lock().map_err(|e| e.to_string())?.push(call);
        Ok(())
    }
}

impl Native for Recorder {
    fn open_url(&self, url: &str) -> Result<(), String> {
        self.record(format!("open_url {url}"))
    }
    fn open_path(&self, path: &Path) -> Result<(), String> {
        self.record(format!("open_path {}", path.display()))
    }
    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.record(format!("reveal {}", path.display()))
    }
    fn pick(&self, _directory: bool, _title: Option<&str>) -> Option<PathBuf> {
        None
    }
    fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        self.record(format!("notify {title} | {body}"))
    }
    fn set_badge(&self, _window: &str, _count: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    fn register_hotkey(&self, chord: &str) -> Result<(), String> {
        if self.taken {
            return Err(format!(
                "{chord} is already registered by another application"
            ));
        }
        self.record(format!("hotkey {chord}"))
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
    fn facts(&self) -> pmt_desktop::platform::Facts {
        pmt_desktop::platform::Facts {
            os: "macos".into(),
            wayland: false,
            sni_host: Some(true),
        }
    }
}

/// A fake bundled binary that logs its arguments to `runs`.
fn binary(dir: &Path) -> PathBuf {
    let path = dir.join("prometheus");
    std::fs::write(
        &path,
        "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/runs\"\necho '{\"error\":{\"message\":\"no\"}}' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn shell(dir: &Path, native: Arc<Recorder>, config: &str) -> Shell {
    Shell::new(
        ShellConfig {
            binary: binary(dir),
            home: dir.to_path_buf(),
            user_home: dir.to_path_buf(),
            ensure_bound: Duration::from_secs(5),
        },
        native as Arc<dyn Native>,
    )
    .with_options(options::parse(config))
}

fn request(value: serde_json::Value) -> Request {
    serde_json::from_value(value).unwrap()
}

/// Scenario: Hotkey already taken.
#[test]
fn a_taken_hotkey_is_named_in_a_notification_and_the_app_runs_on() {
    let dir = tempfile::tempdir().unwrap();
    let native = Arc::new(Recorder {
        taken: true,
        ..Recorder::default()
    });
    let shell = shell(dir.path(), Arc::clone(&native), "");
    let state = shell.register_quick_ask();
    assert_eq!(
        state,
        Hotkey::Taken {
            chord: "CommandOrControl+Shift+Space".into()
        }
    );
    let calls = native.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].starts_with("notify Quick ask hotkey unavailable"));
    assert!(calls[0].contains("CommandOrControl+Shift+Space"));
}

#[test]
fn an_empty_hotkey_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let native = Arc::new(Recorder::default());
    let shell = shell(
        dir.path(),
        Arc::clone(&native),
        "[desktop.quick_ask]\nhotkey = \"\"\n",
    );
    assert_eq!(shell.register_quick_ask(), Hotkey::Disabled);
    assert!(native.calls().is_empty());
}

/// Scenario: Quick follow-up.
#[tokio::test]
async fn a_quick_follow_up_goes_to_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path(), Arc::new(Recorder::default()), "");
    let target = |profile: &str| {
        request(json!({"command": "quick_ask_target", "args": {"profile": profile}}))
    };
    let first = shell.handle("quick-ask", target("default")).await.unwrap();
    assert_eq!(first, json!({"session_id": null}));
    shell
        .handle(
            "quick-ask",
            request(json!({"command": "quick_ask_used", "args": {"profile": "default", "session_id": "ses_A"}})),
        )
        .await
        .unwrap();
    let again = shell.handle("quick-ask", target("default")).await.unwrap();
    assert_eq!(again, json!({"session_id": "ses_A"}));
}

/// Scenario: Background approval (the shell side): the notification
/// names the session and the tool, and its click only hands the window
/// the request to focus.
#[tokio::test]
async fn an_approval_notification_only_ever_focuses_its_card() {
    let dir = tempfile::tempdir().unwrap();
    let native = Arc::new(Recorder::default());
    let shell = shell(dir.path(), Arc::clone(&native), "");
    let approval = json!({"request_id": "srq-1", "session_id": "ses_1", "session_title": "Deploy", "tool": "terminal"});
    shell
        .handle(
            "main",
            request(json!({"command": "notify_approval", "args": approval})),
        )
        .await
        .unwrap();
    assert_eq!(
        native.calls(),
        ["notify Approval needed: terminal | Deploy is waiting for your approval to run terminal."]
    );
    let focused = shell.notices().activated().unwrap();
    assert_eq!(focused.request_id, "srq-1");
    assert!(shell.notices().activated().is_none(), "once");
    assert!(
        !std::fs::exists(dir.path().join("runs")).unwrap(),
        "no daemon call"
    );
}

/// Scenario: Link pointing at an arbitrary URL.
#[tokio::test]
async fn an_install_link_to_a_url_is_unknown_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path(), Arc::new(Recorder::default()), "");
    for command in ["catalog_entry", "catalog_install"] {
        let answer = shell
            .handle(
                "main",
                request(json!({"command": command, "args": {"kind": "plugin", "id": "https://evil.example/p.wasm", "profile": "default"}})),
            )
            .await;
        assert_eq!(
            answer.unwrap_err(),
            "Unknown catalog entry https://evil.example/p.wasm"
        );
    }
    assert!(
        !std::fs::exists(dir.path().join("runs")).unwrap(),
        "nothing ran, nothing was fetched"
    );
}

#[tokio::test]
async fn an_id_missing_from_the_catalog_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path(), Arc::new(Recorder::default()), "");
    let answer = shell
        .handle(
            "main",
            request(json!({"command": "catalog_entry", "args": {"kind": "plugin", "id": "not-there", "profile": "default"}})),
        )
        .await;
    assert_eq!(answer.unwrap_err(), "Unknown catalog entry not-there");
    let runs = std::fs::read_to_string(dir.path().join("runs")).unwrap();
    assert!(
        runs.contains("desktop catalog-entry plugin not-there"),
        "{runs}"
    );
}

/// Scenario: Typo in a launch option (what the window shows).
#[tokio::test]
async fn launch_options_carry_the_notice_of_an_invalid_value() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(
        dir.path(),
        Arc::new(Recorder::default()),
        "[desktop]\ndisable_gpu = \"yes\"\n",
    );
    let answer = shell
        .handle("main", request(json!({"command": "launch_options"})))
        .await
        .unwrap();
    assert_eq!(
        answer["notices"],
        json!([r#"desktop.disable_gpu: expected one of "auto", "on", "off""#])
    );
    assert_eq!(answer["keep_in_tray"], json!(true));
}
