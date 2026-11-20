//! Platform facts, prerequisites and the cursor overlay (spec:
//! computer-use, Platform prerequisites; Agent cursor overlay).
//!
//! macOS needs the Accessibility and Screen Recording grants of the
//! driver's identity (the driver reports them: `permissions status
//! --json`); Linux a reachable X11 display, or Wayland with
//! `computer_use.native_wayland`, and the AT-SPI accessibility bus (on the
//! session D-Bus); Windows an interactive session, not the services'
//! Session 0, unless `computer_use.autostart` registers a per-logon driver
//! task that runs there.

use std::ffi::OsString;

use pmt_config::schema::{ComputerUseConfig, Overlay};
use serde::Serialize;

/// The host facts the checks look at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent host facts
pub struct Facts {
    /// `macos`, `linux`, `windows`...
    pub os: String,
    /// `DISPLAY` is set.
    pub display: bool,
    /// `WAYLAND_DISPLAY` is set.
    pub wayland: bool,
    /// `XDG_SESSION_TYPE = wayland`.
    pub wayland_session: bool,
    /// The session D-Bus is reachable (`DBUS_SESSION_BUS_ADDRESS`).
    pub session_bus: bool,
    /// Linux under WSL.
    pub wsl: bool,
    /// Inside a container.
    pub container: bool,
    /// Windows: an interactive session (`SESSIONNAME` set; services in
    /// Session 0 have none).
    pub interactive: bool,
}

impl Facts {
    /// The facts of an environment where `var` reads a variable.
    #[must_use]
    pub fn of(var: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
        let os = std::env::consts::OS.to_owned();
        let linux = os == "linux";
        Self {
            display: set("DISPLAY"),
            wayland: set("WAYLAND_DISPLAY"),
            wayland_session: var("XDG_SESSION_TYPE").is_some_and(|v| v == "wayland"),
            session_bus: set("DBUS_SESSION_BUS_ADDRESS"),
            wsl: linux
                && std::fs::read_to_string("/proc/version")
                    .is_ok_and(|v| v.to_lowercase().contains("microsoft")),
            container: linux
                && (std::path::Path::new("/.dockerenv").exists()
                    || std::path::Path::new("/run/.containerenv").exists()),
            interactive: set("SESSIONNAME"),
            os,
        }
    }
}

/// Whether the agent cursor overlay is shown: `computer_use.overlay`, or
/// for `auto` off on macOS, headless or WSL Linux, containers and Linux
/// X11 (an unclean end can leave the X11 overlay wedged over every app),
/// on for Linux Wayland and Windows. The overlay is cosmetic.
#[must_use]
pub fn overlay(config: &ComputerUseConfig, facts: &Facts) -> bool {
    match config.overlay {
        Overlay::On => true,
        Overlay::Off => false,
        Overlay::Auto => match facts.os.as_str() {
            "macos" => false,
            "linux" => {
                let headless = !facts.display && !facts.wayland;
                let x11 = facts.display && !facts.wayland && !facts.wayland_session;
                !(headless || facts.wsl || facts.container || x11)
            }
            _ => true,
        },
    }
}

/// One prerequisite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// `accessibility`, `screen_recording`, `display`, `at_spi`,
    /// `interactive_session`.
    pub name: &'static str,
    /// It holds.
    pub granted: bool,
    /// What to do when it does not.
    pub hint: String,
}

/// The prerequisites of this platform; on macOS `driver_report` is the
/// driver's `permissions status --json` (missing when it cannot run).
#[must_use]
pub fn grants(
    config: &ComputerUseConfig,
    facts: &Facts,
    driver_report: Option<&serde_json::Value>,
) -> Vec<Grant> {
    match facts.os.as_str() {
        "macos" => ["accessibility", "screen_recording"]
            .into_iter()
            .map(|name| Grant {
                name,
                granted: driver_report.and_then(|r| r[name].as_bool()) == Some(true),
                hint: format!(
                    "grant {} to CuaDriver in System Settings > Privacy & Security (`prometheus computer-use permissions grant`)",
                    if name == "accessibility" { "Accessibility" } else { "Screen Recording" }
                ),
            })
            .collect(),
        "linux" => {
            let display = facts.display || (facts.wayland && config.native_wayland);
            vec![
                Grant {
                    name: "display",
                    granted: display,
                    hint: if facts.wayland {
                        "no XWayland DISPLAY: set computer_use.native_wayland = true for native Wayland input".to_owned()
                    } else {
                        "no X11 or Wayland display: start the bot screen (`prometheus computer-use screen start`)".to_owned()
                    },
                },
                Grant {
                    name: "at_spi",
                    granted: facts.session_bus,
                    hint: "no session D-Bus for the AT-SPI accessibility bus: run inside a desktop session (or the bot screen)".to_owned(),
                },
            ]
        }
        "windows" => vec![Grant {
            name: "interactive_session",
            granted: facts.interactive || config.autostart,
            hint: "Session 0 (a service) cannot drive the desktop: set computer_use.autostart = true to register a per-logon driver task".to_owned(),
        }],
        _ => Vec::new(),
    }
}

/// The first missing prerequisite, as the tool's unavailable reason.
#[must_use]
pub fn missing(grants: &[Grant]) -> Option<String> {
    grants
        .iter()
        .find(|g| !g.granted)
        .map(|g| format!("{} missing: {}", g.name, g.hint))
}

/// The settings pane of a missing macOS grant.
#[must_use]
pub fn settings_pane(name: &str) -> Option<&'static str> {
    match name {
        "accessibility" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        }
        "screen_recording" => {
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        }
        _ => None,
    }
}

/// The `schtasks` arguments registering the per-logon driver task
/// (`computer_use.autostart` on Windows).
#[must_use]
pub fn autostart_task(driver: &std::path::Path) -> Vec<String> {
    vec![
        "/Create".to_owned(),
        "/F".to_owned(),
        "/SC".to_owned(),
        "ONLOGON".to_owned(),
        "/RL".to_owned(),
        "LIMITED".to_owned(),
        "/TN".to_owned(),
        "PrometheusCuaDriver".to_owned(),
        "/TR".to_owned(),
        format!("\"{}\" serve", driver.display()),
    ]
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::{ComputerUseConfig, Overlay};

    use super::{Facts, grants, missing, overlay};

    fn linux(display: bool, wayland: bool) -> Facts {
        Facts {
            os: "linux".into(),
            display,
            wayland,
            session_bus: true,
            ..Facts::default()
        }
    }

    /// Scenario: X11 desktop.
    #[test]
    fn the_overlay_is_off_on_x11_and_on_on_wayland() {
        let auto = ComputerUseConfig::default();
        assert!(!overlay(&auto, &linux(true, false)), "X11");
        assert!(overlay(&auto, &linux(true, true)), "Wayland with XWayland");
        assert!(!overlay(&auto, &linux(false, false)), "headless");
        assert!(!overlay(
            &auto,
            &Facts {
                os: "macos".into(),
                ..Facts::default()
            }
        ));
        assert!(overlay(
            &auto,
            &Facts {
                os: "windows".into(),
                ..Facts::default()
            }
        ));
        let forced = ComputerUseConfig {
            overlay: Overlay::On,
            ..ComputerUseConfig::default()
        };
        assert!(overlay(&forced, &linux(true, false)));
    }

    #[test]
    fn native_wayland_is_opt_in() {
        let config = ComputerUseConfig::default();
        let only_wayland = linux(false, true);
        assert!(
            missing(&grants(&config, &only_wayland, None))
                .unwrap()
                .starts_with("display missing")
        );
        let native = ComputerUseConfig {
            native_wayland: true,
            ..ComputerUseConfig::default()
        };
        assert_eq!(missing(&grants(&native, &only_wayland, None)), None);
    }

    #[test]
    fn service_sessions_need_autostart() {
        let service = Facts {
            os: "windows".into(),
            ..Facts::default()
        };
        assert!(missing(&grants(&ComputerUseConfig::default(), &service, None)).is_some());
        let autostart = ComputerUseConfig {
            autostart: true,
            ..ComputerUseConfig::default()
        };
        assert_eq!(missing(&grants(&autostart, &service, None)), None);
    }
}
