//! `read_window_below` (spec: desktop-app, Reading the window below): the
//! metadata of the OS window directly beneath the desktop window, never
//! its pixels. Where the OS withholds titles without a permission (macOS
//! Screen Recording) the title is empty with a note naming it; where
//! windows cannot be listed the answer names what would fix it.

use serde::{Deserialize, Serialize};

use crate::hud::{Bounds, Candidate};
use crate::platform::Facts;

/// The macOS permission that reveals other apps' window titles.
pub const MACOS_TITLES: &str = "macOS shows other apps' window titles only with the Screen Recording permission: allow Prometheus in System Settings > Privacy & Security > Screen Recording";

/// What the platform listed: its windows front to back and the desktop
/// window's own place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    /// On-screen windows, front to back.
    pub windows: Vec<Candidate>,
    /// The desktop window.
    pub window: Bounds,
}

/// The window beneath.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Below {
    /// Its application.
    pub app: String,
    /// Its title (empty when the OS withholds it).
    pub title: String,
    /// Where it is.
    pub bounds: Bounds,
    /// The OS's id of the window.
    pub id: String,
}

/// The `read_window_below` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The window beneath (`None`: nothing overlaps the desktop window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Below>,
    /// The application of the frontmost window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmost: Option<String>,
    /// `macos`, `windows` or `linux`.
    pub platform: String,
    /// A permission the OS asks before it shows more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Why nothing could be listed, and what would fix it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The report for `screen` (`None`: the platform listed nothing), where
/// `own_pid` is the desktop app's process.
#[must_use]
pub fn report(facts: &Facts, screen: Option<&Screen>, own_pid: u32) -> Report {
    let platform = facts.os.clone();
    let Some(screen) = screen else {
        return Report {
            window: None,
            frontmost: None,
            error: Some(unavailable(facts).to_owned()),
            platform,
            note: None,
        };
    };
    let ordinary = |w: &&Candidate| w.layer == 0 && w.bounds.width > 0;
    let frontmost = screen.windows.iter().find(ordinary).map(|w| w.app.clone());
    let window = screen
        .windows
        .iter()
        .filter(ordinary)
        .find(|w| w.pid != own_pid && w.bounds.overlaps(&screen.window))
        .map(|w| Below {
            app: w.app.clone(),
            title: w.title.clone(),
            bounds: w.bounds,
            id: w.id.clone(),
        });
    let note = match &window {
        Some(w) if w.title.is_empty() && platform == "macos" => Some(MACOS_TITLES.to_owned()),
        None => Some("no other window is beneath the desktop window".to_owned()),
        Some(_) => None,
    };
    Report {
        window,
        frontmost,
        platform,
        note,
        error: None,
    }
}

/// What would let this platform list windows.
fn unavailable(facts: &Facts) -> &'static str {
    match facts.os.as_str() {
        "linux" if facts.wayland => {
            "native Wayland does not let apps list other windows; start Prometheus Desktop under X11 (desktop.linux_window_backend = \"x11\") to read the window below"
        }
        "linux" => "listing windows needs xprop and xwininfo (the x11-utils package)",
        "macos" => "macOS did not list the on-screen windows (osascript failed)",
        "windows" => "Windows did not list the visible windows (PowerShell failed)",
        _ => "this platform cannot list other windows",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(app: &str, title: &str, pid: u32, x: i64) -> Candidate {
        Candidate {
            app: app.to_owned(),
            title: title.to_owned(),
            pid,
            layer: 0,
            id: format!("{pid}"),
            bounds: Bounds {
                x,
                y: 0,
                width: 400,
                height: 300,
            },
        }
    }

    fn mac() -> Facts {
        Facts {
            os: "macos".to_owned(),
            ..Facts::default()
        }
    }

    /// Scenario: Missing macOS permission.
    #[test]
    fn an_untitled_window_on_macos_names_the_screen_recording_permission() {
        let screen = Screen {
            windows: vec![
                window("Prometheus", "Prometheus", 1, 0),
                window("Safari", "", 2, 100),
            ],
            window: Bounds {
                x: 0,
                y: 0,
                width: 400,
                height: 300,
            },
        };
        let report = report(&mac(), Some(&screen), 1);
        let below = report.window.expect("Safari is beneath");
        assert_eq!((below.app.as_str(), below.title.as_str()), ("Safari", ""));
        assert!(report.note.unwrap().contains("Screen Recording"));
        assert_eq!(report.frontmost.as_deref(), Some("Prometheus"));
    }

    #[test]
    fn no_listing_names_the_fix() {
        let wayland = Facts {
            os: "linux".to_owned(),
            wayland: true,
            ..Facts::default()
        };
        let report = report(&wayland, None, 1);
        assert!(report.error.unwrap().contains("Wayland"));
        assert_eq!(report.platform, "linux");
    }
}
