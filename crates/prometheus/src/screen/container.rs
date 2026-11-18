//! The bot screen in the container backend (spec: execution-environments,
//! Bot screen: `placement = "auto"|"terminal"` with a container backend).
//!
//! The screen gets a desktop container of its own per profile
//! (`pmt-screen-<short id>`, the `-desktop` sandbox image, no network, the
//! container hardening minus the read-only root that Xfce cannot run
//! under). Two directories of the profile's private socket directory are
//! mounted into it: `x11/` as `/tmp/.X11-unix`, where Xvnc creates the X
//! socket of display `:1`, and `rfb/`, where it serves RFB. Both are
//! world-writable so the container's user can create sockets there
//! whatever uid it maps to; the socket directory around them is 0700, so
//! no other user of this host reaches them.
//!
//! The session containers of the profile mount the same `x11/` read-only
//! and get `DISPLAY=:1` ([`for_sessions`]): commands the agent runs there
//! act on the screen. Headed browsers on this host use the X socket by
//! path.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_sandbox::container::{Runtime, first_line, hardening, options::Volume};

use super::launch::{
    Launched, SESSION_SCRIPT, drain_stderr, end, failed, spawn, wait_for_socket, xvnc_args,
};

/// The X display inside the desktop container (it is alone there).
pub const DISPLAY: &str = ":1";
/// Where `x11/` is mounted (the X sockets' standard directory).
const X11_MOUNT: &str = "/tmp/.X11-unix";
/// Where `rfb/` is mounted.
const RFB_MOUNT: &str = "/run/pmt-screen";
/// The label naming the profile whose screen a container is.
const SCREEN_LABEL: &str = "prometheus.screen";

/// The desktop container's name.
#[must_use]
pub fn name(scope: &ProfileScope) -> String {
    format!("pmt-screen-{}", super::short_id(scope))
}

/// The image: `bot_desktop.image`, else the release's desktop variant of
/// the sandbox image.
fn image(scope: &ProfileScope) -> String {
    let configured = &scope.config().bot_desktop.image;
    if configured.is_empty() {
        format!("{}-desktop", crate::chat::session::backend::SANDBOX_IMAGE)
    } else {
        configured.clone()
    }
}

/// Starts the screen in the profile's desktop container.
///
/// # Errors
///
/// `terminal.container_runtime` (no runtime), a hardening flag the
/// runtime rejects without an `allow_degraded` waiver, or
/// `screen.unavailable`.
pub(super) fn launch(
    scope: &ProfileScope,
    sockets: &Path,
    geometry: &str,
) -> Result<Launched, Error> {
    let config = scope.config();
    let runtime = Runtime::detect(&config.terminal.container.runtime)?;
    let x11 = shared_dir(&sockets.join("x11"))?;
    let rfb_dir = shared_dir(&sockets.join("rfb"))?;
    let name = name(scope);
    remove(&runtime, &name);
    create(scope, &runtime, &name, &x11, &rfb_dir)?;
    let mut launched = Launched::new("container", rfb_dir.join("rfb.sock"));
    let cleanup_runtime = runtime.clone();
    let cleanup_name = name.clone();
    launched.cleanup = Some(Box::new(move || remove(&cleanup_runtime, &cleanup_name)));
    let _ = std::fs::remove_file(&launched.rfb);

    let mut xvnc = runtime.command();
    xvnc.args(["exec", &name, "Xvnc", DISPLAY])
        .args(xvnc_args(
            geometry,
            &format!("{RFB_MOUNT}/rfb.sock"),
            "0666",
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut xvnc = spawn(xvnc, "Xvnc in the desktop container")?;
    if let Err(why) = wait_for_socket(&mut xvnc, &launched.rfb, "Xvnc") {
        end(&mut xvnc);
        return Err(failed(&why));
    }
    drain_stderr(&mut xvnc);
    launched.children.push(("Xvnc", xvnc));

    let mut session = runtime.command();
    session
        .args(["exec", &format!("--env=DISPLAY={DISPLAY}"), &name])
        .args(["sh", "-c", SESSION_SCRIPT])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    launched
        .children
        .push(("the desktop session", spawn(session, "xfce4-session")?));
    launched.display = x11.join("X1").display().to_string();
    launched.backend_display = Some(DISPLAY.to_owned());
    Ok(launched)
}

/// Creates and starts the desktop container, dropping a hardening flag
/// the runtime rejects only when `allow_degraded` waives it.
fn create(
    scope: &ProfileScope,
    runtime: &Runtime,
    name: &str,
    x11: &Path,
    rfb_dir: &Path,
) -> Result<(), Error> {
    let config = scope.config();
    let allowed = &config.security.sandbox.allow_degraded;
    let mut flags: Vec<_> = hardening::flags(&config.terminal.container)
        .into_iter()
        .filter(|f| f.name != "read_only_rootfs")
        .collect();
    loop {
        let mut args = vec![
            "run".to_owned(),
            "-d".to_owned(),
            format!("--name={name}"),
            format!("--label={SCREEN_LABEL}={}", scope.name()),
            "--network=none".to_owned(),
            format!("--volume={}:{X11_MOUNT}", x11.display()),
            format!("--volume={}:{RFB_MOUNT}", rfb_dir.display()),
        ];
        args.extend(flags.iter().flat_map(|f| f.args.iter().cloned()));
        args.extend([image(scope), "sleep".to_owned(), "infinity".to_owned()]);
        let output = runtime.run(&args)?;
        if output.status.success() {
            return Ok(());
        }
        remove(runtime, name);
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        let Some(flag) = hardening::rejected(&flags, &stderr) else {
            return Err(failed(&format!(
                "{} cannot create the desktop container: {}",
                runtime.name(),
                first_line(&output.stderr)
            )));
        };
        if !flag.waived_by(allowed) {
            return Err(hardening::unsupported(flag));
        }
        tracing::warn!(
            flag = flag.name,
            "bot screen container: hardening flag dropped (allow_degraded)"
        );
        let dropped = flag.name;
        flags.retain(|f| f.name != dropped);
    }
}

/// Removes the desktop container (and what runs in it), if any.
fn remove(runtime: &Runtime, name: &str) {
    let _ = runtime.run(&["rm".to_owned(), "-f".to_owned(), name.to_owned()]);
}

/// `dir` created world-writable with the sticky bit (see the module).
fn shared_dir(dir: &Path) -> Result<PathBuf, Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let io = |e: std::io::Error| Error::io(&pmt_core::error::codes::HOME_IO, dir, &e);
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    if !std::fs::symlink_metadata(dir).map_err(io)?.is_dir() {
        return Err(failed(&format!("{} is not a directory", dir.display())));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777)).map_err(io)?;
    Ok(dir.to_path_buf())
}

/// What a session container needs to act on the screen.
pub struct SessionDisplay {
    /// The X socket directory, read-only at `/tmp/.X11-unix`.
    pub volume: Volume,
    /// `DISPLAY=:1`.
    pub env: (String, String),
}

/// What a session container of `scope` needs to act on the screen when
/// the screen is placed in the container backend; `None` for another
/// placement.
///
/// # Errors
///
/// `home.io` creating the socket directory.
pub fn for_sessions(scope: &ProfileScope) -> Result<Option<SessionDisplay>, Error> {
    use super::placement::{Hosting, Where};
    let config = scope.config();
    let place = super::placement::resolve(&config.bot_desktop, &config.terminal.backend);
    if !matches!(place, Ok(Where::Backend(Hosting::Container))) {
        return Ok(None);
    }
    let sockets = super::socket_dir(scope);
    super::serve::private_socket_dir(&sockets)?;
    let x11 = shared_dir(&sockets.join("x11"))?;
    Ok(Some(SessionDisplay {
        volume: Volume {
            host: x11,
            container: X11_MOUNT.to_owned(),
            read_only: true,
        },
        env: ("DISPLAY".to_owned(), DISPLAY.to_owned()),
    }))
}
