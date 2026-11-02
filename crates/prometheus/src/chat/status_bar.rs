//! The status bar's client-local commands (spec: tui, Client-local display
//! commands), shared by the TUI and the line interface: `/statusbar`
//! (`/sb`) shows or hides it (`ui.status_bar`), `/battery [on|off|status]`
//! shows the battery as its first item (`ui.status_bar_battery`). Neither
//! reaches the daemon; both persist through the profile's validated config
//! writer. The battery is read on a thread of its own at most once a
//! minute, so a slow reading never holds up a prompt or a frame.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::battery::{self, Battery};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

/// How old a battery reading may get.
const BATTERY_MAX_AGE: Duration = Duration::from_mins(1);

/// The toggles and the battery.
#[derive(Debug, Clone)]
pub struct StatusBar {
    /// `ui.status_bar`.
    pub shown: bool,
    /// `ui.status_bar_battery`.
    pub battery: bool,
    source: Source,
}

#[derive(Debug, Clone)]
enum Source {
    /// The test seam's fixed battery.
    Fixed(Option<Battery>),
    /// The machine's, read in the background.
    Machine(Arc<Mutex<Probe>>),
}

#[derive(Debug, Default)]
struct Probe {
    reading: Option<(Instant, Option<Battery>)>,
    running: bool,
}

impl StatusBar {
    /// The bar as `scope` configures it.
    #[must_use]
    pub fn new(scope: &ProfileScope, launch: &LaunchEnv) -> Self {
        let ui = &scope.config().ui;
        let source = launch
            .test_battery()
            .and_then(battery::from_seam)
            .map_or_else(|| Source::Machine(Arc::default()), Source::Fixed);
        Self {
            shown: ui.status_bar,
            battery: ui.status_bar_battery,
            source,
        }
    }

    /// The battery item's reading when it is on and the machine has one
    /// (the last known reading; a stale one is refreshed in the
    /// background).
    #[must_use]
    pub fn battery_item(&self) -> Option<Battery> {
        if !self.battery {
            return None;
        }
        match &self.source {
            Source::Fixed(found) => *found,
            Source::Machine(probe) => refreshed(probe),
        }
    }

    /// The machine's battery now (`/battery status`).
    #[must_use]
    pub fn read_now(&self) -> Option<Battery> {
        match &self.source {
            Source::Fixed(found) => *found,
            Source::Machine(probe) => {
                let found = battery::read();
                lock(probe).reading = Some((Instant::now(), found));
                found
            }
        }
    }

    /// `/statusbar [on|off]`: sets or flips `ui.status_bar`.
    ///
    /// # Errors
    ///
    /// A bad argument (exit 2) or the config write's errors.
    pub fn statusbar(&mut self, scope: &ProfileScope, args: &str) -> Result<String, Error> {
        let shown = match args.trim() {
            "" => !self.shown,
            "on" => true,
            "off" => false,
            other => return Err(usage(&format!("/statusbar takes on or off, not '{other}'"))),
        };
        persist(scope, "ui.status_bar", shown)?;
        self.shown = shown;
        Ok(if shown {
            "status bar shown (ui.status_bar = true)"
        } else {
            "status bar hidden (ui.status_bar = false)"
        }
        .to_owned())
    }

    /// `/battery [on|off|status]`: sets or flips `ui.status_bar_battery`,
    /// or reports the battery.
    ///
    /// # Errors
    ///
    /// A bad argument (exit 2) or the config write's errors.
    pub fn battery_command(&mut self, scope: &ProfileScope, args: &str) -> Result<String, Error> {
        let on = match args.trim() {
            "status" => {
                return Ok(self
                    .read_now()
                    .map_or_else(|| battery::NONE.to_owned(), Battery::describe));
            }
            "" => !self.battery,
            "on" => true,
            "off" => false,
            other => {
                return Err(usage(&format!(
                    "/battery takes on, off or status, not '{other}'"
                )));
            }
        };
        persist(scope, "ui.status_bar_battery", on)?;
        self.battery = on;
        if !on {
            return Ok("battery hidden (ui.status_bar_battery = false)".to_owned());
        }
        let missing = if self.read_now().is_none() {
            "; no battery detected, so none is shown"
        } else {
            ""
        };
        Ok(format!(
            "battery shown (ui.status_bar_battery = true){missing}"
        ))
    }
}

fn lock(probe: &Mutex<Probe>) -> std::sync::MutexGuard<'_, Probe> {
    probe.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The last reading, starting a background read when it is missing or
/// older than [`BATTERY_MAX_AGE`].
fn refreshed(probe: &Arc<Mutex<Probe>>) -> Option<Battery> {
    let mut probing = lock(probe);
    let old = probing
        .reading
        .is_none_or(|(at, _)| at.elapsed() > BATTERY_MAX_AGE);
    if old && !probing.running {
        probing.running = true;
        let probe = Arc::clone(probe);
        std::thread::spawn(move || {
            let found = battery::read();
            let mut state = lock(&probe);
            state.reading = Some((Instant::now(), found));
            state.running = false;
        });
    }
    probing.reading.and_then(|(_, found)| found)
}

fn persist(scope: &ProfileScope, key: &str, value: bool) -> Result<(), Error> {
    scope
        .profile()
        .config_file()
        .set(key, if value { "true" } else { "false" })
}

fn usage(message: &str) -> Error {
    Error::new(&codes::CLI_USAGE, message.to_owned())
}

/// The line interface's bottom status line: the battery (when shown),
/// the profile and the model, separated like the TUI's.
#[must_use]
pub fn line_text(bar: &StatusBar, profile: &str, model: Option<&str>) -> Option<String> {
    if !bar.shown {
        return None;
    }
    let mut items: Vec<String> = bar.battery_item().map(Battery::item).into_iter().collect();
    items.push(profile.to_owned());
    items.extend(model.map(str::to_owned));
    Some(items.join(" │ "))
}
