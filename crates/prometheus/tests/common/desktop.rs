//! The desktop browser harness: the desktop app's page runs in Playwright's
//! Chromium at the Windows webview origin (`http://tauri.localhost`), and
//! its one `shell` command reaches the real shell core (`pmt-desktop`)
//! through this loopback HTTP server, which runs it with the real
//! `prometheus` binary as the bundled one. What only a native webview
//! does (denied windows, pinned navigation, the IPC allowlist) is covered
//! by the Tauri tests of `apps/desktop/src-tauri`.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_desktop::platform::Facts;
use pmt_desktop::{Native, Request, Shell, ShellConfig, deeplink, ensure, options};
use serde_json::{Value, json};

/// What reached the OS through the shell; `facts` is the machine the
/// platform probes see.
#[derive(Default)]
pub struct Recorder {
    calls: Mutex<Vec<String>>,
    facts: Facts,
}

impl Recorder {
    fn record(&self, call: String) -> Result<(), String> {
        self.calls.lock().map_err(|e| e.to_string())?.push(call);
        Ok(())
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

/// How a harness starts.
pub struct Setup {
    /// The machine the platform probes see.
    pub facts: Facts,
    /// `prometheus://` links that arrive before the window is ready (as
    /// `prometheus desktop` passes them).
    pub links: Vec<String>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            facts: Facts {
                os: std::env::consts::OS.to_owned(),
                wayland: false,
                sni_host: Some(true),
            },
            links: Vec::new(),
        }
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
    fn set_tray(&self, menu: &[pmt_desktop::attention::MenuEntry]) -> Result<(), String> {
        let headings: Vec<&str> = menu
            .iter()
            .filter_map(|e| match e {
                pmt_desktop::attention::MenuEntry::Heading(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        self.record(format!("tray {}", headings.join(" | ")))
    }
    fn facts(&self) -> Facts {
        self.facts.clone()
    }
}

/// The running harness; its URL goes to the specs as
/// `PMT_DESKTOP_HARNESS`.
pub struct Harness {
    pub url: String,
}

/// The "bundled" binary: the test binary on `home` with its isolated user
/// home, as a script the shell runs like the app runs its sidecar.
fn bundled(home: &Path) -> PathBuf {
    let user_home = super::user_home(home);
    let path = home.join("bundled-prometheus");
    let script = format!(
        "#!/bin/sh\nHOME='{}' exec '{}' --home '{}' \"$@\"\n",
        user_home.display(),
        super::BIN,
        home.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Starts the harness for `home`.
pub fn start(home: &Path) -> Harness {
    start_with(home, Setup::default())
}

/// Starts the harness for `home` as `setup` says; the shell reads the
/// default profile's `[desktop]` options as the app does at launch.
pub fn start_with(home: &Path, setup: Setup) -> Harness {
    let recorder = Arc::new(Recorder {
        calls: Mutex::default(),
        facts: setup.facts,
    });
    let shell = Shell::new(
        ShellConfig {
            binary: bundled(home),
            home: home.to_path_buf(),
            user_home: super::user_home(home),
            ensure_bound: ensure::DEADLINE + ensure::GRACE,
        },
        Arc::clone(&recorder) as Arc<dyn Native>,
    )
    .with_options(options::read(&home.join("profiles/default/config.toml")));
    for link in setup.links.iter().filter_map(|l| deeplink::parse(l)) {
        shell.arrived(link);
    }
    let shell = Arc::new(shell);
    let tickets = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for stream in listener.incoming().flatten() {
            let (shell, tickets, recorder) = (
                Arc::clone(&shell),
                Arc::clone(&tickets),
                Arc::clone(&recorder),
            );
            pmt_core::spawn::spawn_on(runtime.handle(), async move {
                serve(stream, &shell, &tickets, &recorder).await;
            });
        }
    });
    Harness { url }
}

async fn serve(
    stream: TcpStream,
    shell: &Shell,
    tickets: &Mutex<Vec<String>>,
    recorder: &Recorder,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let body = super::read_http_request(&mut reader);
    let answer = match path.split('?').next().unwrap_or("/") {
        "/shell" => shell_call(shell, tickets, &path, &body).await,
        "/deeplink" => json!(deeplink::parse(&String::from_utf8_lossy(&body))),
        "/tickets" => json!(*tickets.lock().unwrap()),
        "/calls" => json!(recorder.calls()),
        // The app activated after a notification click: what the window
        // is handed (the Tauri app emits it as `focus-request`).
        "/activate" => json!(shell.notices().activated()),
        "/plugin-frame" => {
            let id = path.split("id=").nth(1).unwrap_or_default();
            return match shell.panels().document(id) {
                Ok(html) => respond_html(stream, &html),
                Err(e) => respond(stream, &json!({"err": e})),
            };
        }
        _ => json!({"err": "no such harness route"}),
    };
    respond(stream, &answer);
}

/// Runs one shell request for the window named in the query; the answer
/// is `{"ok": value}` or `{"err": message}`. Issued tickets are recorded
/// so a spec can look for them in the page's storage.
async fn shell_call(shell: &Shell, tickets: &Mutex<Vec<String>>, path: &str, body: &[u8]) -> Value {
    let window = path.split("window=").nth(1).unwrap_or("main");
    let request: Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => return json!({"err": format!("rejected: {e}")}),
    };
    match shell.handle(window, request).await {
        Ok(value) => {
            if let Some(protocols) = value["protocols"].as_array() {
                let mut issued = tickets.lock().unwrap();
                for p in protocols.iter().filter_map(Value::as_str) {
                    if let Some(ticket) = p.strip_prefix("prometheus-ticket.") {
                        issued.push(ticket.to_owned());
                    }
                }
            }
            json!({"ok": value})
        }
        Err(message) => json!({"err": message}),
    }
}

fn respond_html(mut stream: TcpStream, html: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-security-policy: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        pmt_desktop::panels::POLICY,
        html.len()
    );
}

fn respond(mut stream: TcpStream, answer: &Value) {
    let body = answer.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}
