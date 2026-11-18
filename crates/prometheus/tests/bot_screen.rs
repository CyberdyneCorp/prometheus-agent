//! The bot screen against stand-in Xvnc and Xfce programs (spec:
//! execution-environments, Bot screen; Bot screen takeover lease):
//! `prometheus computer-use screen`, the display stream behind a
//! single-use display ticket, and the takeover lease over RPC.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use common::{append_config, command, mock_home, ok, run, stderr};
use futures_util::{SinkExt as _, StreamExt as _};
use pmt_rpc::contracts as c;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

fn fakes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot-screen")
}

/// `PATH` with the stand-ins first.
fn path_with_fakes() -> OsString {
    let mut dirs = vec![fakes()];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(dirs).unwrap()
}

fn screen(home: &Path, args: &[&str]) -> Output {
    command(home)
        .args(["computer-use", "screen"])
        .args(args)
        .env("PATH", path_with_fakes())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn status(home: &Path) -> serde_json::Value {
    let out = command(home)
        .args(["--json", "computer-use", "screen", "status"])
        .env("PATH", path_with_fakes())
        .output()
        .unwrap();
    serde_json::from_str(&ok(&out)).unwrap()
}

/// A home whose screen needs no free memory (CI runners vary).
fn home() -> tempfile::TempDir {
    let home = mock_home();
    append_config(home.path(), "\n[bot_desktop]\nmin_free_memory_bytes = 0\n");
    home
}

fn alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

struct Stopped<'a>(&'a Path);

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        let _ = screen(self.0, &["stop", "--force"]);
    }
}

#[test]
fn start_status_and_stop_the_screen() {
    let home = home();
    let _guard = Stopped(home.path());
    ok(&screen(home.path(), &["start"]));
    let shown = status(home.path());
    assert_eq!(shown["running"], true, "{shown}");
    assert_eq!(shown["screen"]["geometry"], "1440x900");
    assert_eq!(shown["holder"], serde_json::Value::Null);
    let display = shown["screen"]["display"].as_str().unwrap().to_owned();
    assert!(display.starts_with(':'), "{display}");

    // RFB on a 0600 Unix socket only, never TCP.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("profiles/default/run/screen/screen.json"))
            .unwrap(),
    )
    .unwrap();
    let rfb = PathBuf::from(record["rfb"].as_str().unwrap());
    let mode = std::fs::metadata(&rfb).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(format!("{}.argv", rfb.display())).unwrap())
            .unwrap();
    assert_eq!(argv[0], display);
    let rfbport = argv.iter().position(|a| a == "-rfbport").unwrap();
    assert_eq!(argv[rfbport + 1], "-1");
    assert!(argv.windows(2).any(|w| w == ["-geometry", "1440x900"]));

    // A second start returns the running screen.
    ok(&screen(home.path(), &["start"]));
    let pid = record["pid"].as_u64().unwrap();
    ok(&screen(home.path(), &["stop"]));
    assert_eq!(status(home.path())["running"], false);
    let gone = (0..50).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        !alive(pid)
    });
    assert!(gone, "the supervisor exited");
    assert!(!rfb.exists(), "its socket is removed");
}

/// Scenario: Low memory (the floor above what any host has free).
#[test]
fn too_little_free_memory_refuses_naming_the_threshold() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[bot_desktop]\nmin_free_memory_bytes = 1125899906842624\n",
    );
    let out = screen(home.path(), &["start"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(stderr(&out).contains("1073741824 MB"), "{}", stderr(&out));
    assert_eq!(status(home.path())["running"], false);
}

/// Scenario: Sandbox that cannot host a display.
#[test]
fn a_backend_that_cannot_host_a_display_points_to_gateway_placement() {
    let home = home();
    ok(&run(
        home.path(),
        &["config", "set", "terminal.backend", "modal"],
    ));
    let out = screen(home.path(), &["start"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("bot_desktop.placement = \"gateway\""),
        "{}",
        stderr(&out)
    );
    assert_eq!(status(home.path())["running"], false);
}

#[test]
fn an_unused_screen_stops_after_its_idle_time() {
    let home = home();
    let _guard = Stopped(home.path());
    append_config(home.path(), "idle_stop_secs = 1\n");
    ok(&screen(home.path(), &["start"]));
    let deadline = Instant::now() + Duration::from_secs(20);
    while status(home.path())["running"] == true {
        assert!(Instant::now() < deadline, "the idle screen stopped");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn stop_waits_for_a_person_unless_forced() {
    let home = home();
    let _guard = Stopped(home.path());
    ok(&screen(home.path(), &["start"]));
    let lease = pmt_config::screen_lease::ScreenLease::of(&home.path().join("profiles/default"));
    lease
        .take("desktop", pmt_config::screen_lease::now())
        .unwrap();
    assert_eq!(status(home.path())["holder"], "desktop");
    let out = screen(home.path(), &["stop"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains("desktop"), "{}", stderr(&out));
    assert_eq!(status(home.path())["running"], true);
    ok(&screen(home.path(), &["stop", "--force"]));
    let after = status(home.path());
    assert_eq!(after["running"], false);
    assert_eq!(
        after["holder"],
        serde_json::Value::Null,
        "--force clears the lease"
    );
}

#[test]
fn install_without_root_or_sudo_prints_the_exact_command() {
    let home = home();
    // Only the stand-in apt-get: no sudo to be found.
    let out = command(home.path())
        .args(["computer-use", "screen", "install", "-y"])
        .env("PATH", fakes())
        .output()
        .unwrap();
    let printed = ok(&out);
    if nix::unistd::geteuid().is_root() {
        return; // root installs directly
    }
    assert!(
        printed.contains("apt-get install -y --no-install-recommends tigervnc-standalone-server"),
        "{printed}"
    );
    assert!(
        stderr(&out).contains("packages: tigervnc-standalone-server"),
        "{}",
        stderr(&out)
    );
}

/// The desktop's view: a display ticket opens the RFB stream once; the
/// takeover lease moves between the viewer and the agent over RPC.
#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_streams_the_screen_and_takes_it_over() {
    let home = home();
    let _guard = Stopped(home.path());
    let _daemon = common::daemon::start(home.path(), &[]);
    ok(&screen(home.path(), &["start"]));
    let (client, _inbox) = common::daemon::desktop(home.path(), "default", &[]).await;

    let ticket: c::ScreenTicket = serde_json::from_value(
        client
            .call("screen.ticket", serde_json::json!({}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ticket.path, "/screen");
    assert_eq!(ticket.expires_in_secs, 30);
    let endpoint = common::daemon::record(home.path()).endpoint;
    let url = endpoint.rpc_url.replace("/rpc", &ticket.path);
    let open = |ticket: String| {
        let mut request = url.as_str().into_client_request().unwrap();
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!("prometheus-display.v1, prometheus-ticket.{ticket}")
                .parse()
                .unwrap(),
        );
        tokio_tungstenite::connect_async(request)
    };
    let (mut socket, _) = open(ticket.ticket.clone()).await.unwrap();
    let Some(Ok(Message::Binary(greeting))) = socket.next().await else {
        panic!("no RFB greeting");
    };
    assert_eq!(&greeting[..], b"RFB 003.008\n");
    socket
        .send(Message::binary(b"RFB 003.008\n".to_vec()))
        .await
        .unwrap();
    let Some(Ok(Message::Binary(security))) = socket.next().await else {
        panic!("no security types");
    };
    assert_eq!(&security[..], [1, 1], "one type offered: None");
    socket.close(None).await.unwrap();

    // Single use: the same ticket is refused.
    let (mut again, _) = open(ticket.ticket).await.unwrap();
    match again.next().await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 4401),
        other => panic!("a reused ticket is refused, got {other:?}"),
    }

    let taken: c::ScreenLeaseState = serde_json::from_value(
        client
            .call("screen.takeover", serde_json::json!({"viewer": "laptop"}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(taken.holder.as_deref(), Some("laptop"));
    assert_eq!(status(home.path())["holder"], "laptop");
    let refused = client
        .call("screen.handback", serde_json::json!({"viewer": "phone"}))
        .await
        .unwrap_err();
    assert!(refused.message.contains("laptop"), "{refused:?}");
    let back: c::ScreenLeaseState = serde_json::from_value(
        client
            .call("screen.handback", serde_json::json!({"viewer": "laptop"}))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(back.holder, None);
    assert_eq!(back.epoch, taken.epoch);
}

/// While the screen runs, a headed browser gets its display and the
/// persistent profile the desktop's browser launcher shares; using it
/// counts as use of the screen.
#[test]
fn a_headed_browser_acts_on_the_running_screen() {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    let home = home();
    let _guard = Stopped(home.path());
    let scope = || {
        let root = Home::open(home.path()).unwrap();
        let profile = root.profile(&ProfileName::default_profile()).unwrap();
        ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
    };
    assert!(
        prometheus::screen::for_headed_browser(&scope()).is_none(),
        "no screen, no auto start"
    );
    ok(&screen(home.path(), &["start"]));
    let display = status(home.path())["screen"]["display"]
        .as_str()
        .unwrap()
        .to_owned();
    let used = home.path().join("profiles/default/run/screen/used");
    let before = std::fs::metadata(&used).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let record = prometheus::screen::for_headed_browser(&scope()).unwrap();
    assert_eq!(record.display, display);
    assert!(std::fs::metadata(&used).unwrap().modified().unwrap() > before);
    assert_eq!(
        prometheus::screen::browser_profile(&scope()),
        home.path()
            .join("profiles/default/cache/browser/screen-profile")
    );
}

/// With `bot_desktop.auto_start` on, the first headed browser launch of a
/// session starts the screen and the browser runs on its display and
/// shared profile (the stand-in browser prints both and exits; the
/// browser's confinement keeps it from writing anywhere else).
#[test]
fn the_first_headed_browser_launch_auto_starts_the_screen() {
    let home = home();
    let _guard = Stopped(home.path());
    let browser = home.path().join("fake-browser");
    std::fs::write(
        &browser,
        "#!/bin/sh\necho \"display=$DISPLAY args=$*\" >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
    append_config(
        home.path(),
        &format!(
            "auto_start = true\n\n[browser]\nheaded = true\nexecutable_path = {:?}\n",
            browser.display().to_string()
        ),
    );
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "browse"
[[rule.tool_calls]]
name = "browser_exec"
arguments = { code = "print(page_info())" }

[[rule]]
repeat = true
text = "done"
"#,
    )
    .unwrap();
    assert_eq!(status(home.path())["running"], false);
    let out = command(home.path())
        .current_dir(home.path())
        .env("PATH", path_with_fakes())
        .args(["-z", "browse", "--yolo", "--toolsets", "terminal,browser"])
        .arg("--mock-script")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    ok(&out);
    let shown = status(home.path());
    assert_eq!(shown["running"], true, "the launch started it: {shown}");
    let display = shown["screen"]["display"].as_str().unwrap();

    let session = common::newest_session(home.path());
    let messages = common::session_messages(home.path(), &session);
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .expect("a browser_exec result");
    assert!(
        result.contains(&format!("display={display} ")),
        "the browser ran on the screen's display: {result}"
    );
    assert!(
        result.contains("cache/browser/screen-profile"),
        "on the screen's shared profile: {result}"
    );
}
