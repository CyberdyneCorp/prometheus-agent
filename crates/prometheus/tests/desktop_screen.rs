//! The desktop's bot screen pane end to end (spec: execution-environments,
//! Bot screen; Bot screen takeover lease): the page in the desktop browser
//! harness against the real daemon and a bot screen run by stand-in Xvnc
//! and Xfce programs (tests/fixtures/bot-screen), driving
//! `web/e2e/desktop-screen.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::ffi::OsString;
use std::path::Path;

use common::dashboard::playwright;
use common::{append_config, command, mock_home, ok, run};

struct Stop<'a>(&'a Path, OsString);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = command(self.0)
            .args(["computer-use", "screen", "stop", "--force"])
            .env("PATH", &self.1)
            .output();
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// The person takes the screen over from the desktop, opens the browser
/// launcher on it and hands it back; the view is read-only while the
/// agent has control.
#[test]
fn desktop_screen_pane_streams_and_takes_over() {
    let home = mock_home();
    let fakes = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot-screen");
    append_config(
        home.path(),
        &format!(
            "\n[browser]\nexecutable_path = \"{}\"\n[bot_desktop]\nmin_free_memory_bytes = 0\ngeometry = \"120x100\"\n",
            fakes.join("launcher/chromium").display()
        ),
    );
    let mut dirs = vec![fakes];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(dirs).unwrap();
    let _stop = Stop(home.path(), path.clone());
    ok(&command(home.path())
        .args(["computer-use", "screen", "start"])
        .env("PATH", &path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap());
    let harness = common::desktop::start(home.path());
    playwright(
        home.path(),
        "desktop-screen.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}
