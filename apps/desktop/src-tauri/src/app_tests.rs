//! Tauri tests of the shell's IPC boundary (spec: desktop-app, Native
//! command allowlist; Windows never open themselves; External URL scheme
//! allowlist; Deep links), on Tauri's mock runtime with the app's real
//! capability file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_desktop::{Native, Shell, ShellConfig};
use serde_json::{Value, json};
use tauri::Listener as _;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder};
use tauri::webview::{InvokeRequest, NewWindowResponse};
use tauri::{Manager as _, WebviewWindow, WebviewWindowBuilder};

use super::{DEEP_LINK_EVENT, MAIN, deliver, new_window, with_shell};

/// Records what reached the OS.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl Recorder {
    fn record(&self, call: String) -> Result<(), String> {
        self.calls.lock().map_err(|e| e.to_string())?.push(call);
        Ok(())
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
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
    fn notify(&self, title: &str, _body: &str) -> Result<(), String> {
        self.record(format!("notify {title}"))
    }
    fn set_badge(&self, window: &str, _count: Option<u32>) -> Result<(), String> {
        self.record(format!("badge {window}"))
    }
    fn register_hotkey(&self, chord: &str) -> Result<(), String> {
        self.record(format!("hotkey {chord}"))
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
}

const APP_URL: &str = if cfg!(windows) {
    "http://tauri.localhost"
} else {
    "tauri://localhost"
};

fn app(recorder: &Arc<Recorder>) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
    let home = std::env::temp_dir();
    let shell = Shell::new(
        ShellConfig {
            binary: PathBuf::from("prometheus-not-run-here"),
            home: home.clone(),
            user_home: home,
            ensure_bound: std::time::Duration::from_secs(1),
        },
        Arc::clone(recorder) as Arc<dyn Native>,
    );
    let app = with_shell(mock_builder())
        .manage(Arc::new(shell))
        .build(tauri::generate_context!(test = true))
        .expect("mock app");
    let window = WebviewWindowBuilder::new(&app, MAIN, tauri::WebviewUrl::default())
        .build()
        .expect("main window");
    (app, window)
}

fn invoke(
    window: &WebviewWindow<MockRuntime>,
    cmd: &str,
    from: &str,
    body: Value,
) -> Result<Value, Value> {
    get_ipc_response(
        window,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: from.parse().unwrap(),
            body: InvokeBody::Json(body),
            headers: tauri::http::HeaderMap::default(),
            invoke_key: INVOKE_KEY.to_owned(),
        },
    )
    .map(|r| r.deserialize::<Value>().unwrap())
}

#[test]
fn a_javascript_link_from_the_agent_is_invalid_and_opens_nothing() {
    let recorder = Arc::new(Recorder::default());
    let (_app, window) = app(&recorder);
    let answer = invoke(
        &window,
        "shell",
        APP_URL,
        json!({"request": {"command": "open_external", "args": {"url": "javascript:alert(1)"}}}),
    );
    assert_eq!(answer, Ok(json!({"ok": false, "reason": "invalid"})));
    let answer = invoke(
        &window,
        "shell",
        APP_URL,
        json!({"request": {"command": "open_external", "args": {"url": "https://example.com/"}}}),
    );
    assert_eq!(answer, Ok(json!({"ok": true})));
    assert_eq!(
        recorder.calls(),
        vec!["open_url https://example.com/".to_owned()]
    );
}

#[test]
fn only_the_typed_shell_command_is_reachable() {
    let recorder = Arc::new(Recorder::default());
    let (_app, window) = app(&recorder);
    // A command outside the allowlist, a plugin's own command and a
    // request of the wrong shape are all rejected.
    let unknown = invoke(&window, "reveal_in_folder", APP_URL, json!({"path": "/"}));
    assert!(unknown.is_err(), "{unknown:?}");
    let plugin = invoke(
        &window,
        "plugin:opener|reveal_item_in_dir",
        APP_URL,
        json!({"path": "/"}),
    );
    assert!(plugin.is_err(), "{plugin:?}");
    let shaped = invoke(
        &window,
        "shell",
        APP_URL,
        json!({"request": {"command": "run", "args": {"cmd": "ls"}}}),
    );
    assert!(shaped.unwrap_err().to_string().contains("rejected"));
    assert!(recorder.calls().is_empty());
}

#[test]
fn a_plugin_frame_has_no_native_commands() {
    let recorder = Arc::new(Recorder::default());
    let (_app, window) = app(&recorder);
    // A plugin frame is served from another origin than the app's: the
    // capability does not cover it, so its invoke never reaches the shell.
    let home = std::env::temp_dir();
    let frame = "http://127.0.0.1:9120/api/plugin-frames/a";
    let answer = invoke(
        &window,
        "shell",
        frame,
        json!({"request": {"command": "reveal_in_folder", "args": {"path": home}}}),
    );
    let refusal = answer.unwrap_err().to_string();
    assert!(refusal.contains("not allowed"), "{refusal}");
    assert!(recorder.calls().is_empty());
}

#[test]
fn a_plugin_refusal_is_answered_without_touching_the_os() {
    let recorder = Arc::new(Recorder::default());
    let (_app, window) = app(&recorder);
    let answer = invoke(
        &window,
        "shell",
        APP_URL,
        json!({"request": {"command": "plugin_refused", "args": {"plugin": "a", "command": "reveal_in_folder"}}}),
    );
    assert_eq!(answer, Ok(json!({"ok": false, "reason": "plugin"})));
    assert!(recorder.calls().is_empty());
}

#[test]
fn an_artifact_script_opens_no_window() {
    let url: tauri::Url = "https://attacker.example".parse().unwrap();
    assert!(matches!(
        new_window::<MockRuntime>(&url),
        NewWindowResponse::Deny
    ));
}

#[test]
fn a_deep_link_with_a_prompt_payload_opens_an_empty_draft() {
    let recorder = Arc::new(Recorder::default());
    let (app, _window) = app(&recorder);
    let (tx, rx) = std::sync::mpsc::channel();
    app.listen_any(DEEP_LINK_EVENT, move |event| {
        tx.send(event.payload().to_owned()).unwrap();
    });
    deliver(
        app.handle(),
        "prometheus://new?profile=work&prompt=rm%20-rf",
    );
    let payload: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(payload, json!({"open": "new", "profile": "work"}));
    deliver(app.handle(), "prometheus://settings/reset");
    assert!(rx.try_recv().is_err());
    assert!(app.get_webview_window(MAIN).is_some());
}
