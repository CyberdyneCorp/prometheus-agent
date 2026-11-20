//! The desktop status bar on a managed local model (spec: local-models,
//! Local resource telemetry): the page in the desktop browser harness
//! (`common/desktop.rs`) against the real daemon on the `pmt-llama-stub`
//! engine, driving `web/e2e/desktop-local.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use common::dashboard::playwright;
use common::desktop::Setup;
use common::local::{FIXTURES_VAR, fixtures, local_home};
use common::run;

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// Local resource telemetry: the GPU utilisation, GPU memory and RAM the
/// session status reports show on the desktop status bar.
#[test]
fn the_desktop_status_bar_shows_local_resources() {
    let home = local_home();
    let h = home.path();
    let _stop = Stop(h);
    let setup = Setup {
        env: vec![(
            FIXTURES_VAR.to_owned(),
            fixtures(h).to_string_lossy().into_owned(),
        )],
        ..Setup::default()
    };
    let harness = common::desktop::start_with(h, setup);
    playwright(
        h,
        "desktop-local.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}
