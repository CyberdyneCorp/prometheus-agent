//! The bot screen on the gateway host (the `local` backend, or
//! `bot_desktop.placement = "gateway"`): Xvnc on a free display with RFB
//! on a 0600 Unix socket only, then the Xfce session on that display.

use std::path::Path;
use std::process::{Command, Stdio};

use pmt_core::error::Error;

use super::launch::{
    Launched, SESSION_SCRIPT, drain_stderr, end, failed, spawn, wait_for_socket, xvnc_args,
};

/// The display numbers tried.
const DISPLAYS: std::ops::Range<u32> = 50..100;

/// Starts the screen with its RFB socket in `sockets`.
///
/// # Errors
///
/// `screen.unavailable`: Xvnc or the session did not start.
pub(super) fn launch(sockets: &Path, geometry: &str) -> Result<Launched, Error> {
    let mut launched = Launched::new("host", sockets.join("rfb.sock"));
    let (xvnc, display) = start_xvnc(&launched.rfb, geometry)?;
    launched.children.push(("Xvnc", xvnc));
    let mut session = Command::new("sh");
    session
        .args(["-c", SESSION_SCRIPT])
        .env("DISPLAY", &display)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    launched
        .children
        .push(("the desktop session", spawn(session, "xfce4-session")?));
    launched.display = display;
    Ok(launched)
}

/// A display number nobody holds: no X lock file and no X socket.
fn free_displays() -> impl Iterator<Item = u32> {
    DISPLAYS.filter(|n| {
        !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
    })
}

/// Starts Xvnc on the first free display that it accepts.
fn start_xvnc(rfb: &Path, geometry: &str) -> Result<(std::process::Child, String), Error> {
    let mut last = String::from("no free display between :50 and :99");
    let rfb_text = rfb.to_string_lossy();
    for number in free_displays().take(5) {
        let display = format!(":{number}");
        let _ = std::fs::remove_file(rfb);
        let mut command = Command::new("Xvnc");
        command
            .arg(&display)
            .args(xvnc_args(geometry, &rfb_text, "0600"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut xvnc = spawn(command, "Xvnc")?;
        match wait_for_socket(&mut xvnc, rfb, "Xvnc") {
            Ok(()) => {
                drain_stderr(&mut xvnc);
                return Ok((xvnc, display));
            }
            Err(why) => {
                end(&mut xvnc);
                last = why;
            }
        }
    }
    Err(failed(&last))
}
