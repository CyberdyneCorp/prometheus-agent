//! `prometheus computer-use screen serve`: the bot screen's supervisor
//! (see the parent module). It starts the screen where
//! `bot_desktop.placement` puts it (this host, the container backend or the
//! SSH host), records it and answers `status` and `stop` on its control
//! socket until a stop, the end of a process the screen needs, or
//! `bot_desktop.idle_stop_secs` without use; then it ends the screen and
//! removes its files.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use super::Record;
use super::launch::{Launched, failed};
use super::placement::{Hosting, Where};

/// How often the supervisor looks at its children and the idle time.
const TICK: Duration = Duration::from_millis(200);

/// Runs the screen of `scope` until it stops.
///
/// # Errors
///
/// `screen.unavailable`: the screen did not start; `home.io`.
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
    let config = scope.config();
    let geometry = config.bot_desktop.geometry.clone();
    let mut launched =
        match super::placement::resolve(&config.bot_desktop, &config.terminal.backend)? {
            Where::Host => super::host::launch(&sockets, &geometry)?,
            Where::Backend(Hosting::Container) => {
                super::container::launch(scope, &sockets, &geometry)?
            }
            Where::Backend(Hosting::Ssh) => super::ssh::launch(scope, &sockets, &geometry)?,
        };
    let record = Record {
        pid: std::process::id(),
        display: launched.display.clone(),
        backend_display: launched.backend_display.clone(),
        rfb: launched.rfb.clone(),
        xauthority: Some(launched.xauthority.clone()),
        geometry,
        placement: launched.placement.to_owned(),
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
    let why = supervise(scope, &record, &control, &mut launched);
    tracing::info!(profile = %scope.name(), "bot screen stopped: {why}");
    drop(launched);
    super::remove_files(scope);
    for path in [record.rfb.clone(), super::used_path(scope)] {
        let _ = std::fs::remove_file(path);
    }
    // The container placement keeps the directory: the profile's session
    // containers mount its `x11/`.
    if record.placement != "container" {
        let _ = std::fs::remove_dir_all(&sockets);
    }
    Ok(0)
}

/// Answers the control socket until a reason to stop.
fn supervise(
    scope: &ProfileScope,
    record: &Record,
    control: &UnixListener,
    screen: &mut Launched,
) -> String {
    let idle_stop = scope.config().bot_desktop.idle_stop_secs;
    let mut last_check = Instant::now();
    loop {
        match control.accept() {
            Ok((stream, _)) => {
                if answer(scope, record, &stream) {
                    return "stop requested".to_owned();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(TICK),
            Err(_) => return "control socket failed".to_owned(),
        }
        if last_check.elapsed() < Duration::from_secs(1) {
            continue;
        }
        last_check = Instant::now();
        if let Some(name) = screen.exited() {
            return format!("{name} exited");
        }
        if idle_stop > 0 && super::idle_secs(scope).is_some_and(|idle| idle >= idle_stop) {
            return "idle".to_owned();
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
        "backend_display": record.backend_display,
        "started_at": record.started_at,
        "idle_secs": super::idle_secs(scope),
    })
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
///
/// # Errors
///
/// `home.io`, or `screen.unavailable` for a directory that is not private.
pub fn private_socket_dir(dir: &Path) -> Result<(), Error> {
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
