//! The launcher entry point. On Windows a confined child is started by a
//! second copy of the calling executable run as
//! `<exe> __pmt-sandbox-launch ...` (the standard library cannot create
//! `AppContainer` processes), so every executable that builds sandboxed
//! commands calls [`run_if_requested`] first thing in `main`.

use std::ffi::OsString;

/// The first argument that turns an executable into the launcher.
pub const LAUNCH_ARG: &str = "__pmt-sandbox-launch";

/// Exit status of a launcher that could not start its child (as `env`,
/// `timeout` and container runtimes use for "the wrapper failed").
pub const LAUNCH_FAILED: i32 = 125;

/// Runs the launcher when this process was started as one: returns the
/// exit status to exit with, or `None` for a normal start.
#[must_use]
pub fn run_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next()?;
    let rest: Vec<OsString> = args.collect();
    let result = if first == LAUNCH_ARG {
        launch(&rest)
    } else if is_relay(&first) {
        relay(&rest)
    } else {
        return None;
    };
    Some(match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("prometheus sandbox launcher: {err}");
            LAUNCH_FAILED
        }
    })
}

#[cfg(windows)]
fn is_relay(arg: &OsString) -> bool {
    arg == crate::platform::relay::RELAY_ARG
}

#[cfg(not(windows))]
fn is_relay(_arg: &OsString) -> bool {
    false
}

#[cfg(windows)]
fn relay(args: &[OsString]) -> Result<i32, String> {
    match args {
        [pipe] => crate::platform::relay::serve(pipe),
        _ => Err("usage: __pmt-sandbox-relay <pipe>".to_owned()),
    }
}

#[cfg(not(windows))]
fn relay(_args: &[OsString]) -> Result<i32, String> {
    Err("the egress relay is only used on Windows".to_owned())
}

#[cfg(windows)]
fn launch(args: &[OsString]) -> Result<i32, String> {
    let spec = crate::platform::LaunchSpec::parse(args)?;
    // Exit codes are DWORDs (NTSTATUS values included); keep the bits.
    crate::platform::launch::run(&spec).map(u32::cast_signed)
}

#[cfg(not(windows))]
fn launch(_args: &[OsString]) -> Result<i32, String> {
    Err("the sandbox launcher is only used on Windows".to_owned())
}
