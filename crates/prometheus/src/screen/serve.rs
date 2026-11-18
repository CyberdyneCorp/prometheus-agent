//! `prometheus computer-use screen serve`: the bot screen's supervisor
//! (see the parent module). It starts Xvnc on a free display with RFB on
//! a 0600 Unix socket only, then the Xfce session on that display, records
//! itself and answers `status` and `stop` on its control socket until a
//! stop, the end of either child, or `bot_desktop.idle_stop_secs` without
//! use; then it ends both process groups and removes its files.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use super::Record;

/// How long Xvnc may take to create its RFB socket.
const XVNC_START: Duration = Duration::from_secs(15);
/// How often the supervisor looks at its children and the idle time.
const TICK: Duration = Duration::from_millis(200);
/// The display numbers tried.
const DISPLAYS: std::ops::Range<u32> = 50..100;

struct Screen {
    xvnc: Child,
    session: Child,
}

impl Drop for Screen {
    fn drop(&mut self) {
        for child in [&mut self.session, &mut self.xvnc] {
            pmt_tools::browser::cleanup::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn failed(why: &str) -> Error {
    Error::new(
        &codes::SCREEN_UNAVAILABLE,
        format!("the bot screen cannot start: {why}"),
    )
}

/// Runs the screen of `scope` until it stops.
///
/// # Errors
///
/// `screen.unavailable`: Xvnc or the session did not start; `home.io`.
pub fn serve(scope: &ProfileScope) -> Result<u8, Error> {
    let dir = super::dir(scope);
    pmt_core::private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    pmt_core::private_fs::restrict_to_owner(&dir)
        .map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    if super::running(scope).is_some() {
        return Err(failed("a screen of this profile is already running"));
    }
    let sockets = super::socket_dir(scope);
    private_socket_dir(&sockets)?;
    let control_path = super::control_path(scope);
    let _ = std::fs::remove_file(&control_path);
    let control = UnixListener::bind(&control_path)
        .map_err(|e| Error::io(&codes::HOME_IO, &control_path, &e))?;
    control
        .set_nonblocking(true)
        .map_err(|e| Error::io(&codes::HOME_IO, &control_path, &e))?;
    let geometry = scope.config().bot_desktop.geometry.clone();
    let rfb = sockets.join("rfb.sock");
    let (mut xvnc, display) = start_xvnc(&rfb, &geometry)?;
    let session = match start_session(&display) {
        Ok(session) => session,
        Err(e) => {
            pmt_tools::browser::cleanup::kill_group(xvnc.id());
            let _ = xvnc.kill();
            let _ = xvnc.wait();
            return Err(e);
        }
    };
    let mut screen = Screen { xvnc, session };
    let record = Record {
        pid: std::process::id(),
        display,
        rfb,
        geometry,
        placement: "host".to_owned(),
        started_at: jiff::Timestamp::now().to_string(),
    };
    write_record(scope, &record)?;
    std::fs::write(super::used_path(scope), b"")
        .map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{}", super::READY);
        let _ = stdout.flush();
    }
    let why = supervise(scope, &record, &control, &mut screen);
    tracing::info!(profile = %scope.name(), "bot screen stopped: {why}");
    drop(screen);
    super::remove_files(scope);
    for path in [record.rfb.clone(), super::used_path(scope)] {
        let _ = std::fs::remove_file(path);
    }
    Ok(0)
}

/// Answers the control socket until a reason to stop.
fn supervise(
    scope: &ProfileScope,
    record: &Record,
    control: &UnixListener,
    screen: &mut Screen,
) -> &'static str {
    let idle_stop = scope.config().bot_desktop.idle_stop_secs;
    let mut last_check = Instant::now();
    loop {
        match control.accept() {
            Ok((stream, _)) => {
                if answer(scope, record, &stream) {
                    return "stop requested";
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(TICK),
            Err(_) => return "control socket failed",
        }
        if last_check.elapsed() < Duration::from_secs(1) {
            continue;
        }
        last_check = Instant::now();
        if !matches!(screen.xvnc.try_wait(), Ok(None)) {
            return "Xvnc exited";
        }
        if !matches!(screen.session.try_wait(), Ok(None)) {
            return "the desktop session exited";
        }
        if idle_stop > 0 && super::idle_secs(scope).is_some_and(|idle| idle >= idle_stop) {
            return "idle";
        }
    }
}

/// Handles one control request; `true` to stop. Only the profile's OS
/// user reaches the socket (its directory is 0700).
fn answer(scope: &ProfileScope, record: &Record, stream: &UnixStream) -> bool {
    let timeout = Some(Duration::from_secs(2));
    if stream.set_nonblocking(false).is_err()
        || stream.set_read_timeout(timeout).is_err()
        || stream.set_write_timeout(timeout).is_err()
    {
        return false;
    }
    let mut line = String::new();
    let Ok(clone) = stream.try_clone() else {
        return false;
    };
    let _ = BufReader::new(clone.take(4096)).read_line(&mut line);
    let request: Value = serde_json::from_str(&line).unwrap_or_default();
    let (reply, stop) = match request["op"].as_str().unwrap_or_default() {
        "status" => (status(scope, record), false),
        "stop" => (json!({"stopping": true}), true),
        other => (json!({"error": format!("unknown op '{other}'")}), false),
    };
    let mut stream = stream;
    let _ = stream.write_all(format!("{reply}\n").as_bytes());
    stop
}

fn status(scope: &ProfileScope, record: &Record) -> Value {
    json!({
        "pid": record.pid,
        "display": record.display,
        "geometry": record.geometry,
        "placement": record.placement,
        "started_at": record.started_at,
        "idle_secs": super::idle_secs(scope),
    })
}

/// A display number nobody holds: no X lock file and no X socket.
fn free_displays() -> impl Iterator<Item = u32> {
    DISPLAYS.filter(|n| {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    })
}

/// Starts Xvnc on the first free display that it accepts.
fn start_xvnc(rfb: &Path, geometry: &str) -> Result<(Child, String), Error> {
    let mut last = String::from("no free display between :50 and :99");
    for number in free_displays().take(5) {
        let display = format!(":{number}");
        let _ = std::fs::remove_file(rfb);
        let mut command = Command::new("Xvnc");
        command
            .arg(&display)
            .args(["-geometry", geometry, "-depth", "24"])
            .arg("-rfbunixpath")
            .arg(rfb)
            .args(["-rfbunixmode", "0600", "-rfbport", "-1"])
            .args([
                "-SecurityTypes",
                "None",
                "-nolisten",
                "tcp",
                "-AlwaysShared",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        pmt_tools::browser::cleanup::new_group(&mut command);
        let mut xvnc = command.spawn().map_err(|e| failed(&format!("Xvnc: {e}")))?;
        match wait_for_socket(&mut xvnc, rfb) {
            Ok(()) => return Ok((xvnc, display)),
            Err(why) => {
                pmt_tools::browser::cleanup::kill_group(xvnc.id());
                let _ = xvnc.kill();
                let _ = xvnc.wait();
                last = why;
            }
        }
    }
    Err(failed(&last))
}

/// Waits for Xvnc's RFB socket.
fn wait_for_socket(xvnc: &mut Child, rfb: &Path) -> Result<(), String> {
    let deadline = Instant::now() + XVNC_START;
    while Instant::now() < deadline {
        if rfb.exists() {
            return Ok(());
        }
        if let Ok(Some(status)) = xvnc.try_wait() {
            let mut why = String::new();
            if let Some(mut stderr) = xvnc.stderr.take() {
                let _ = stderr.read_to_string(&mut why);
            }
            let last = why
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            return Err(format!("Xvnc exited ({status}): {last}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "Xvnc did not create its socket within {} s",
        XVNC_START.as_secs()
    ))
}

/// Starts the Xfce session on `display` (under its own D-Bus session bus
/// when `dbus-launch` is installed).
fn start_session(display: &str) -> Result<Child, Error> {
    let path = std::env::var_os("PATH");
    let dbus = super::install::find("dbus-launch", path.as_deref());
    let mut command = match dbus {
        Some(dbus) => {
            let mut command = Command::new(dbus);
            command.args(["--exit-with-session", "xfce4-session"]);
            command
        }
        None => Command::new("xfce4-session"),
    };
    command
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    pmt_tools::browser::cleanup::new_group(&mut command);
    command
        .spawn()
        .map_err(|e| failed(&format!("xfce4-session: {e}")))
}

fn write_record(scope: &ProfileScope, record: &Record) -> Result<(), Error> {
    let path: PathBuf = super::record_path(scope);
    let _ = std::fs::remove_file(&path);
    let mut file = pmt_core::private_fs::new_file_options()
        .open(&path)
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    file.write_all(&serde_json::to_vec(record).expect("a record serializes"))
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
}

/// Creates the socket directory 0700, or accepts an existing one only when
/// this user owns it and nobody else can enter it (it lives in the shared
/// system temp).
fn private_socket_dir(dir: &Path) -> Result<(), Error> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let io = |e: std::io::Error| Error::io(&codes::HOME_IO, dir, &e);
    match pmt_core::private_fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    let meta = std::fs::symlink_metadata(dir).map_err(io)?;
    let ours = meta.uid() == nix::unistd::geteuid().as_raw();
    if !meta.is_dir() || !ours || meta.permissions().mode() & 0o077 != 0 {
        return Err(failed(&format!(
            "{} is not a private directory of this user",
            dir.display()
        )));
    }
    Ok(())
}
