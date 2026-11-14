//! The management panes, the messaging pane and Telegram quick setup, the
//! Projects sidebar's discovery, shortcuts, the palette and profile files
//! end to end (spec: desktop-app, Management panes; Messaging pane shows
//! saved versus running state; Telegram quick setup by QR; Repository
//! discovery for the Projects sidebar; Fonts, shortcuts, palette and
//! profile files): the page in the desktop browser harness against the
//! real daemon, driving `web/e2e/desktop-manage.spec.ts`. The Telegram
//! onboarding service and Bot API are local stand-ins.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::dashboard::playwright;
use common::desktop::Setup;
use common::{append_config, mock_home, ok, run, run_with_stdin};
use serde_json::{Value, json};

const SECRET: &str = "sk-desktop-export-secret-0123456789";
const BOT_TOKEN: &str = "123456789:AAFakeTokenForTheOnboardingTest_-xyz";

struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// Serves each request with `answer(request line, body)` as JSON.
fn serve(answer: fn(&str, &[u8]) -> Value) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || respond(stream, answer));
        }
    });
    port
}

fn respond(mut stream: TcpStream, answer: fn(&str, &[u8]) -> Value) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some(n) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let text = answer(&line, &body).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// The token a cancelled setup's late result carries (never stored).
const LATE_TOKEN: &str = "987654321:BBLateTokenOfACancelledSetup_-abcdef";

/// The first pairing (the cancelled setup) holds each poll until the spec
/// releases it, then answers ready; later pairings are ready on their
/// second poll.
static CREATED: AtomicUsize = AtomicUsize::new(0);
static LATER_POLLS: AtomicUsize = AtomicUsize::new(0);
static RELEASED: AtomicBool = AtomicBool::new(false);
static DELIVERED: AtomicBool = AtomicBool::new(false);

/// Below the daemon's 10 s request timeout, so a held poll is answered.
const HOLD: Duration = Duration::from_secs(8);

fn wait_for(flag: &AtomicBool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !flag.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    flag.load(Ordering::SeqCst)
}

/// The onboarding service; `POST /release` lets the held poll of the
/// first pairing answer and says whether it did.
fn onboarding(line: &str, _body: &[u8]) -> Value {
    if line.starts_with("POST /v1/telegram/pairings ") {
        let n = CREATED.fetch_add(1, Ordering::SeqCst) + 1;
        return json!({"pairing_id": format!("p{n}"), "poll_token": "poll-secret",
                      "suggested_username": "my_prometheus_bot",
                      "deep_link": "https://t.me/newbot/SetupBot/my_prometheus_bot"});
    }
    if line.starts_with("POST /release ") {
        RELEASED.store(true, Ordering::SeqCst);
        return json!({"delivered": wait_for(&DELIVERED, HOLD)});
    }
    if line.contains("/v1/telegram/pairings/p1 ") {
        if !wait_for(&RELEASED, HOLD) {
            return json!({"status": "pending"});
        }
        DELIVERED.store(true, Ordering::SeqCst);
        return json!({"status": "ready", "token": LATE_TOKEN, "bot_username": "late_bot", "owner_user_id": 9999});
    }
    if LATER_POLLS.fetch_add(1, Ordering::SeqCst) >= 1 {
        json!({"status": "ready", "token": BOT_TOKEN, "bot_username": "my_prometheus_bot", "owner_user_id": 4242})
    } else {
        json!({"status": "pending"})
    }
}

/// The Bot API: the bot exists, no updates arrive, every other call works.
fn bot_api(line: &str, _body: &[u8]) -> Value {
    let method = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    match method.split('?').next().unwrap_or_default() {
        "getMe" => {
            json!({"ok": true, "result": {"id": 1, "is_bot": true, "first_name": "Prometheus", "username": "my_prometheus_bot"}})
        }
        "getUpdates" => {
            std::thread::sleep(std::time::Duration::from_millis(500));
            json!({"ok": true, "result": []})
        }
        _ => json!({"ok": true, "result": true}),
    }
}

fn mkdirs(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
}

/// The member texts of a profile archive (`.tar.zst`).
fn archive_members(path: &Path) -> Vec<(String, String)> {
    let decoder = zstd::Decoder::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().display().to_string();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (name, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect()
}

/// The profiles, jobs, secrets, repositories and services the spec uses.
fn prepare(home: &Path, onboarding_port: u16) {
    for profile in ["work", "home"] {
        ok(&run(home, &["profile", "create", profile]));
    }
    ok(&run(
        home,
        &[
            "-p",
            "home",
            "--json",
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "water the plants",
            "--yes",
        ],
    ));
    ok(&run_with_stdin(
        home,
        &["secrets", "set", "SLACK_BOT_TOKEN"],
        "xoxb-saved-token\n",
    ));
    ok(&run_with_stdin(
        home,
        &["-p", "work", "secrets", "set", "OPENAI_API_KEY"],
        &format!("{SECRET}\n"),
    ));
    let code = common::user_home(home).join("code");
    for repo in ["alpha", "beta", "gamma"] {
        mkdirs(&code.join(repo).join(".git"));
    }
    ok(&run(
        home,
        &[
            "projects",
            "create",
            "gamma",
            code.join("gamma").to_str().unwrap(),
        ],
    ));
    append_config(
        home,
        &format!(
            "\n[projects]\nrepo_scan_roots = [\"{}\"]\n\n[desktop]\nfont_family = \"Fira Sans\"\n\n\
             [gateway]\nmultiplex = true\n\n[gateway.platforms.telegram.options]\nsetup_service_url = \"http://127.0.0.1:{onboarding_port}\"\n",
            code.display()
        ),
    );
}

/// Scenario: Pausing another profile's job. Scenario: Saved but not
/// started. Scenario: Cancelled setup. Scenario: Successful QR setup.
/// Scenario: Scanning disabled.
/// Scenario: Conflicting shortcut. Scenario: Exported profile has no keys.
#[test]
fn desktop_management_messaging_projects_shortcuts_and_profile_files() {
    let home = mock_home();
    let setup = serve(onboarding);
    prepare(home.path(), setup);
    let setup = format!("http://127.0.0.1:{setup}");
    let api = format!("http://127.0.0.1:{}", serve(bot_api));
    let export = home.path().join("exports/work.tar.zst");
    mkdirs(export.parent().unwrap());
    let _stop = Stop(home.path());
    let harness = common::desktop::start_with(
        home.path(),
        Setup {
            save_to: Some(export.clone()),
            env: vec![("PMT_TEST_TELEGRAM_API".to_owned(), api)],
            ..Setup::default()
        },
    );
    playwright(
        home.path(),
        "desktop-manage.spec.ts",
        &[
            ("PMT_DESKTOP_HARNESS", harness.url.as_str()),
            ("PMT_TELEGRAM_SETUP", setup.as_str()),
        ],
    );
    let members = archive_members(&export);
    assert!(members.iter().any(|(name, _)| name == "config.toml"));
    assert!(
        members
            .iter()
            .any(|(name, _)| name == "desktop-appearance.json")
    );
    for (name, text) in &members {
        assert!(!text.contains(SECRET), "{name} carries a secret value");
    }
    let allow = ok(&run(
        home.path(),
        &["config", "get", "gateway.platforms.telegram.allow_users"],
    ));
    assert!(allow.contains("4242") && !allow.contains(','), "{allow}");
    assert!(!allow.contains("9999"), "{allow}");
    let secrets = ok(&run(home.path(), &["--json", "secrets", "list"]));
    assert!(secrets.contains("TELEGRAM_BOT_TOKEN"), "{secrets}");
}
