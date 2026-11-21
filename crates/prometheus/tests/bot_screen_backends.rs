//! The bot screen placed in the terminal backend (spec:
//! execution-environments, Bot screen: `bot_desktop.placement = "auto"`
//! puts it inside a container or SSH backend), against a stand-in Docker
//! CLI and SSH client whose "container" and "remote host" are this machine
//! (tests/fixtures/bot-screen/backends) and the stand-in Xvnc and Xfce.
#![cfg(unix)]

mod common;

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::{append_config, command, mock_home, ok, stderr};

fn fakes() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot-screen")
}

/// `PATH` with the backend stand-ins and then `extra` first.
fn path_with(extra: &[PathBuf]) -> OsString {
    let mut dirs = vec![fakes().join("backends")];
    dirs.extend(extra.iter().cloned());
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(dirs).unwrap()
}

struct Backend {
    home: common::TestHome,
    /// The stand-ins' state and call logs (`XDG_RUNTIME_DIR`).
    state: PathBuf,
    path: OsString,
}

impl Backend {
    fn new(config: &str) -> Self {
        let home = mock_home();
        append_config(
            home.path(),
            &format!("\n{config}\n[bot_desktop]\nmin_free_memory_bytes = 0\n"),
        );
        let state = home.path().join("fake-state");
        std::fs::create_dir_all(&state).unwrap();
        Self {
            home,
            state,
            path: path_with(&[fakes()]),
        }
    }

    fn screen(&self, args: &[&str]) -> Output {
        command(self.home.path())
            .args(["--json", "computer-use", "screen"])
            .args(args)
            .env("PATH", &self.path)
            .env("XDG_RUNTIME_DIR", &self.state)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn status(&self) -> serde_json::Value {
        serde_json::from_str(&ok(&self.screen(&["status"]))).unwrap()
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.screen(&["stop", "--force"]);
    }
}

/// The greeting of the screen's RFB socket on this host.
fn greeting(rfb: &str) -> Vec<u8> {
    let mut stream = UnixStream::connect(rfb).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut greeting = [0u8; 12];
    stream.read_exact(&mut greeting).unwrap();
    greeting.to_vec()
}

#[test]
fn auto_places_the_screen_in_a_desktop_container() {
    let docker = fakes().join("backends/docker");
    let backend = Backend::new(&format!(
        "[terminal]\nbackend = \"container\"\n[terminal.container]\nruntime = \"{}\"",
        docker.display()
    ));
    ok(&backend.screen(&["start"]));
    let shown = backend.status();
    assert_eq!(shown["running"], true, "{shown}");
    let screen = &shown["screen"];
    assert_eq!(screen["placement"], "container");
    assert_eq!(screen["backend_display"], ":1");
    let display = screen["display"].as_str().unwrap();
    assert!(display.ends_with("/x11/X1"), "{display}");

    // The desktop container: the release's desktop image, no network,
    // hardened (minus the read-only root), the X and RFB socket
    // directories mounted.
    let calls = backend.log("fake-docker.log");
    let run: Vec<String> = calls
        .lines()
        .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
        .find(|args| args[0] == "run")
        .unwrap();
    assert!(run.contains(&"--network=none".to_owned()), "{run:?}");
    assert!(run.contains(&"--cap-drop=ALL".to_owned()), "{run:?}");
    assert!(
        run.contains(&"--security-opt=no-new-privileges".to_owned()),
        "{run:?}"
    );
    assert!(!run.contains(&"--read-only".to_owned()), "{run:?}");
    assert!(
        run.iter()
            .any(|a| a.starts_with("ghcr.io/") && a.ends_with("-desktop")),
        "{run:?}"
    );
    assert!(
        run.iter().any(|a| a.ends_with(":/tmp/.X11-unix")),
        "{run:?}"
    );
    assert!(
        run.iter().any(|a| a.ends_with("/xauth:/run/pmt-xauth:ro")),
        "{run:?}"
    );

    // Xvnc serves RFB inside it, reached on this host.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            backend
                .home
                .path()
                .join("profiles/default/run/screen/screen.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let rfb = record["rfb"].as_str().unwrap().to_owned();
    assert_eq!(greeting(&rfb), b"RFB 003.008\n");
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(format!("{rfb}.argv")).unwrap()).unwrap();
    assert_eq!(argv[0], ":1");
    assert!(argv.windows(2).any(|w| w == ["-rfbport", "-1"]), "{argv:?}");
    let auth = argv.iter().position(|a| a == "-auth").expect("-auth");
    assert!(argv[auth + 1].ends_with("/xauth/Xauthority"), "{argv:?}");
    assert!(Path::new(&argv[auth + 1]).is_file());

    ok(&backend.screen(&["stop"]));
    assert_eq!(backend.status()["running"], false);
    assert!(
        backend
            .log("fake-docker.log")
            .lines()
            .any(|l| l.starts_with("[\"rm\"")),
        "the desktop container is removed"
    );
    assert!(!Path::new(&rfb).exists());
}

#[test]
fn a_hardening_flag_the_runtime_rejects_fails_without_a_waiver() {
    let docker = fakes().join("backends/docker");
    let backend = Backend::new(&format!(
        "[terminal]\nbackend = \"container\"\n[terminal.container]\nruntime = \"{}\"",
        docker.display()
    ));
    std::fs::write(backend.state.join("reject"), "--pids-limit").unwrap();
    let out = backend.screen(&["start"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("pids_limit"), "{}", stderr(&out));
    assert_eq!(backend.status()["running"], false);
    // Waived, the flag is dropped and the screen starts.
    append_config(
        backend.home.path(),
        "[security.sandbox]\nallow_degraded = [\"all\"]\n",
    );
    ok(&backend.screen(&["start"]));
    assert_eq!(backend.status()["screen"]["placement"], "container");
}

/// The session containers of the profile see the screen.
#[test]
fn session_containers_share_the_screen_display() {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    let backend = Backend::new("[terminal]\nbackend = \"container\"");
    let root = Home::open(backend.home.path()).unwrap();
    let profile = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    let shared = prometheus::screen::container::for_sessions(&scope)
        .unwrap()
        .unwrap();
    assert_eq!(shared.volumes[0].container, "/tmp/.X11-unix");
    assert!(shared.volumes[0].host.ends_with("x11"));
    assert_eq!(shared.volumes[1].container, "/run/pmt-xauth");
    assert!(shared.volumes[1].host.ends_with("xauth"));
    assert!(shared.volumes.iter().all(|v| v.read_only));
    assert_eq!(
        shared.env,
        [
            ("DISPLAY".to_owned(), ":1".to_owned()),
            (
                "XAUTHORITY".to_owned(),
                "/run/pmt-xauth/Xauthority".to_owned()
            )
        ]
    );
}

const SSH: &str =
    "[terminal]\nbackend = \"ssh\"\n[terminal.ssh]\nhost = \"bot.example\"\nuser = \"agent\"";

#[test]
fn auto_places_the_screen_on_the_ssh_host() {
    let backend = Backend::new(SSH);
    ok(&backend.screen(&["start"]));
    let shown = backend.status();
    assert_eq!(shown["running"], true, "{shown}");
    let screen = &shown["screen"];
    assert_eq!(screen["placement"], "ssh");
    let remote = screen["backend_display"].as_str().unwrap().to_owned();
    assert!(remote.starts_with(':'), "{remote}");
    let display = screen["display"].as_str().unwrap();
    assert!(
        display.ends_with(&format!("/x11/X{}", &remote[1..])),
        "{display}"
    );

    // RFB relayed from the host's private socket to this host.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            backend
                .home
                .path()
                .join("profiles/default/run/screen/screen.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let rfb = record["rfb"].as_str().unwrap().to_owned();
    assert_eq!(greeting(&rfb), b"RFB 003.008\n");
    let calls = backend.log("fake-ssh.log");
    assert!(calls.contains("agent@bot.example"), "{calls}");
    let forward = calls.lines().find(|l| l.contains(" -N ")).unwrap();
    assert!(forward.contains(&format!("-L {rfb}:")), "{forward}");
    // Xvnc on the SSH host admits X clients by the cookie written there;
    // this host's clients get a private copy.
    let remote_rfb = forward
        .split("-L ")
        .nth(1)
        .unwrap()
        .split([':', ' '])
        .nth(1)
        .unwrap();
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(format!("{remote_rfb}.argv")).unwrap())
            .unwrap();
    let auth = argv.iter().position(|a| a == "-auth").expect("-auth");
    let remote_auth = Path::new(&argv[auth + 1]);
    assert!(remote_auth.ends_with("Xauthority"), "{argv:?}");
    let local_auth = record["xauthority"].as_str().unwrap();
    assert_eq!(
        std::fs::read(remote_auth).unwrap(),
        std::fs::read(local_auth).unwrap()
    );

    ok(&backend.screen(&["stop"]));
    assert_eq!(backend.status()["running"], false);
    assert!(!Path::new(&rfb).exists());
}

#[test]
fn an_ssh_host_without_xvnc_is_refused_naming_it() {
    let mut backend = Backend::new(SSH);
    // Only the stand-in SSH client and the base system: no Xvnc.
    backend.path = std::env::join_paths([
        fakes().join("backends"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let out = command(backend.home.path())
        .args(["computer-use", "screen", "start"])
        .env("PATH", &backend.path)
        .env("XDG_RUNTIME_DIR", &backend.state)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Xvnc is not installed on agent@bot.example"),
        "{}",
        stderr(&out)
    );
}
