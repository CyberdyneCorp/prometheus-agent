//! The bot screen on the SSH backend's host (spec: execution-environments,
//! Bot screen: `placement = "auto"|"terminal"` with the ssh backend).
//!
//! Xvnc and the Xfce session run on the SSH host as the SSH user, in a
//! private directory `${TMPDIR:-/tmp}/pmt-bot-<short id>` there (0700,
//! owned by that user) that holds the RFB socket (0600), the screen's X
//! cookie (`Xauthority`, 0600; X clients on the SSH host need it) and their
//! pids. A forwarding client of its own relays that RFB socket and the
//! display's X socket to the profile's private socket directory here, so
//! the gateway streams it and headed browsers on this host draw on it with
//! a copy of the cookie. The SSH host decides the free-memory floor and
//! must have Xvnc and Xfce installed.

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_sandbox::ssh::SshTarget;

use super::launch::{
    Launched, SESSION_SCRIPT, XVNC_START, drain_stderr, end, failed, last_line, spawn,
    wait_for_socket, xvnc_args,
};

/// Checks the SSH host's programs, then makes the screen's directory
/// there (`$1`) and prints it with the host's free memory.
const PREPARE: &str = r#"set -e
for program in Xvnc xfce4-session; do
  command -v "$program" >/dev/null 2>&1 || { echo "missing:$program"; exit 0; }
done
dir="${TMPDIR:-/tmp}/$1"
umask 077
mkdir -p "$dir"
[ -O "$dir" ] && [ ! -L "$dir" ] || { echo "$dir is not a directory of this user" >&2; exit 1; }
chmod 700 "$dir"
awk '/^MemAvailable:/ { print "free:" $2 * 1024 }' /proc/meminfo 2>/dev/null || true
echo "dir:$dir""#;

/// Starts Xvnc on the first free display of the SSH host (`$1`: the
/// directory, then the Xvnc arguments after the display); prints the
/// display before it becomes Xvnc.
const XVNC: &str = r#"dir="$1"; shift
n=50
while [ "$n" -lt 100 ]; do
  if [ ! -e "/tmp/.X$n-lock" ] && [ ! -e "/tmp/.X11-unix/X$n" ]; then
    echo $$ > "$dir/xvnc.pid"
    echo ":$n"
    exec Xvnc ":$n" "$@"
  fi
  n=$((n + 1))
done
echo "no free display between :50 and :99" >&2
exit 1"#;

/// Starts the session on `$2` recording its pid in `$1`.
const SESSION: &str = r#"echo $$ > "$1/session.pid"; DISPLAY="$2"; XAUTHORITY="$1/Xauthority"; export DISPLAY XAUTHORITY; "#;

/// Writes stdin to the cookie file in `$1`, private to the SSH user.
const WRITE_COOKIE: &str = r#"umask 077; cat > "$1/Xauthority""#;

/// Ends what the screen left on the SSH host (`$1`: the directory).
const CLEANUP: &str =
    r#"for f in "$1"/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null; done; rm -rf "$1""#;

/// What [`PREPARE`] reported.
#[derive(Debug, Default, PartialEq, Eq)]
struct Prepared {
    dir: String,
    missing: Option<String>,
    free: Option<u64>,
}

fn parse_prepared(stdout: &str) -> Prepared {
    let mut prepared = Prepared::default();
    for line in stdout.lines() {
        if let Some(dir) = line.strip_prefix("dir:") {
            dir.clone_into(&mut prepared.dir);
        } else if let Some(program) = line.strip_prefix("missing:") {
            prepared.missing = Some(program.to_owned());
        } else if let Some(free) = line.strip_prefix("free:") {
            prepared.free = free.trim().parse().ok();
        }
    }
    prepared
}

/// Starts the screen on the SSH host.
///
/// # Errors
///
/// `terminal.backend_unavailable` (no SSH destination or client), or
/// `screen.unavailable`: the host lacks memory or programs, or a step did
/// not start.
pub(super) fn launch(
    scope: &ProfileScope,
    sockets: &Path,
    geometry: &str,
) -> Result<Launched, Error> {
    let config = scope.config();
    let control = scope.path("run/ssh");
    pmt_core::private_fs::create_dir(&control).ok();
    let target = SshTarget::from_config(&config.terminal.ssh, Some(&control))?;
    let host = target.destination().to_owned();
    let prepared = prepare(&target, &format!("pmt-bot-{}", super::short_id(scope)))?;
    if let Some(program) = &prepared.missing {
        return Err(failed(&format!("{program} is not installed on {host}"))
            .with_hint("install TigerVNC's Xvnc and xfce4-session on the SSH host (see `prometheus computer-use screen install` for the packages)"));
    }
    super::memory::check(prepared.free, config.bot_desktop.min_free_memory_bytes)?;
    let remote = prepared.dir;
    let cookie = super::xauth::fresh()?;
    write_remote_cookie(&target, &remote, &cookie)?;
    let local_auth = sockets.join(super::xauth::FILE);
    super::xauth::write(&local_auth, &cookie, false)?;
    let mut launched = Launched::new("ssh", sockets.join("rfb.sock"), local_auth);
    let cleanup_target = target.clone();
    let cleanup_dir = remote.clone();
    launched.cleanup = Some(Box::new(move || {
        let mut command = cleanup_target.command(&["sh", "-c", CLEANUP, "sh", &cleanup_dir]);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let _ = pmt_sandbox::container::run_bounded(command, Duration::from_secs(20));
    }));

    let remote_rfb = format!("{remote}/rfb.sock");
    let mut argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        XVNC.to_owned(),
        "sh".to_owned(),
        remote.clone(),
    ];
    argv.extend(xvnc_args(
        geometry,
        &remote_rfb,
        "0600",
        &format!("{remote}/{}", super::xauth::FILE),
    ));
    let mut xvnc = target.command(&argv);
    xvnc.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut xvnc = spawn(xvnc, &format!("Xvnc on {host}"))?;
    let Some(display) = first_stdout_line(&mut xvnc) else {
        end(&mut xvnc);
        let mut why = String::new();
        if let Some(mut stderr) = xvnc.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut stderr, &mut why);
        }
        return Err(failed(&format!(
            "Xvnc on {host} did not start: {}",
            last_line(&why)
        )));
    };
    drain_stderr(&mut xvnc);
    launched.children.push(("Xvnc", xvnc));
    wait_remote_socket(&target, &remote_rfb)?;

    let session_script = format!("{SESSION}{SESSION_SCRIPT}");
    let mut session = target.command(&["sh", "-c", &session_script, "sh", &remote, &display]);
    session.stdout(Stdio::null()).stderr(Stdio::null());
    launched
        .children
        .push(("the desktop session", spawn(session, "xfce4-session")?));

    let x11 = sockets.join("x11");
    pmt_core::private_fs::create_dir(&x11).or_else(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => Ok(()),
        _ => Err(Error::io(&pmt_core::error::codes::HOME_IO, &x11, &e)),
    })?;
    let local_x = x11.join(format!("X{}", display.trim_start_matches(':')));
    let mut forward = target.socket_forwards(&[
        (launched.rfb.clone(), remote_rfb),
        (
            local_x.clone(),
            format!("/tmp/.X11-unix/X{}", display.trim_start_matches(':')),
        ),
    ]);
    forward.stderr(Stdio::piped());
    let _ = std::fs::remove_file(&launched.rfb);
    let mut forward = spawn(forward, &format!("the SSH forward to {host}"))?;
    if let Err(why) = wait_for_socket(&mut forward, &launched.rfb, "the SSH forward") {
        end(&mut forward);
        return Err(failed(&why));
    }
    drain_stderr(&mut forward);
    launched.children.push(("the SSH forward", forward));
    launched.display = local_x.display().to_string();
    launched.backend_display = Some(display);
    Ok(launched)
}

/// Writes `cookie` to the screen's directory `remote` on the SSH host.
fn write_remote_cookie(target: &SshTarget, remote: &str, cookie: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    let mut command = target.command(&["sh", "-c", WRITE_COOKIE, "sh", remote]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let why = |e: &dyn std::fmt::Display| {
        failed(&format!(
            "cannot write the X cookie on {}: {e}",
            target.destination()
        ))
    };
    let mut child = command.spawn().map_err(|e| why(&e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(cookie).map_err(|e| why(&e))?;
    }
    let output = child.wait_with_output().map_err(|e| why(&e))?;
    if !output.status.success() {
        return Err(why(&last_line(&String::from_utf8_lossy(&output.stderr))));
    }
    Ok(())
}

/// Runs [`PREPARE`] for the directory `name`.
fn prepare(target: &SshTarget, name: &str) -> Result<Prepared, Error> {
    let command = target.command(&["sh", "-c", PREPARE, "sh", name]);
    let output = pmt_sandbox::container::run_bounded(command, Duration::from_mins(1))
        .map_err(|why| failed(&format!("{}: {why}", target.destination())))?;
    let prepared = parse_prepared(&String::from_utf8_lossy(&output.stdout));
    let ready = prepared.missing.is_some() || !prepared.dir.is_empty();
    if !output.status.success() || !ready {
        return Err(failed(&format!(
            "cannot prepare the screen on {}: {}",
            target.destination(),
            last_line(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(prepared)
}

/// The display line Xvnc's launcher prints, or `None` when it exited
/// first (its stderr says why).
fn first_stdout_line(child: &mut std::process::Child) -> Option<String> {
    let stdout = child.stdout.take()?;
    let (sender, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let _ = sender.send(lines.next().and_then(Result::ok));
        // Keep draining: Xvnc must never block on a full pipe.
        for _ in lines {}
    });
    received
        .recv_timeout(XVNC_START)
        .ok()
        .flatten()
        .map(|line| line.trim().to_owned())
        .filter(|line| line.starts_with(':'))
}

/// Waits for Xvnc's RFB socket on the SSH host.
fn wait_remote_socket(target: &SshTarget, socket: &str) -> Result<(), Error> {
    let deadline = Instant::now() + XVNC_START;
    while Instant::now() < deadline {
        let mut test = target.command(&["test", "-S", socket]);
        test.stdout(Stdio::null()).stderr(Stdio::null());
        if test.status().is_ok_and(|s| s.success()) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(failed(&format!(
        "Xvnc on {} did not create its socket within {} s",
        target.destination(),
        XVNC_START.as_secs()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_host_reports_is_read_line_by_line() {
        let prepared = parse_prepared("free:1048576\ndir:/tmp/pmt-bot-ab\n");
        assert_eq!(
            prepared,
            Prepared {
                dir: "/tmp/pmt-bot-ab".into(),
                missing: None,
                free: Some(1_048_576)
            }
        );
        assert_eq!(
            parse_prepared("missing:Xvnc\n").missing.as_deref(),
            Some("Xvnc")
        );
    }
}
