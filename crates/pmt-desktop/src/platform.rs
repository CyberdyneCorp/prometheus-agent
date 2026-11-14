//! The shell's own platform probes (spec: desktop-app, Honest platform
//! support in settings): which desktop features this machine cannot offer
//! and why, so Settings disables their controls instead of letting them
//! fail at use. The daemon's `platform_support` rows (sandbox, containers,
//! keychain...) come over the protocol; these rows are the window's.

use serde::Serialize;

/// The global shortcut feature (the quick-ask hotkey).
pub const GLOBAL_SHORTCUT: &str = "desktop.global_shortcut";
/// The tray icon (background running after the last window closes).
pub const TRAY: &str = "desktop.tray";
/// The dock or taskbar badge.
pub const BADGE: &str = "desktop.badge";
/// HUD placement: snapping the bar to the pointer.
pub const HUD_SNAP: &str = "desktop.hud.snap";
/// HUD context: the window underneath the bar.
pub const HUD_BELOW: &str = "desktop.hud.window_below";
/// Why HUD placement is off on native Wayland.
pub const WAYLAND_PLACEMENT: &str = "the compositor controls window placement";
/// Why the window underneath is unknown on native Wayland.
pub const WAYLAND_WINDOWS: &str = "the compositor does not tell applications about other windows";
/// The reason a Wayland session gives for the hotkey.
pub const WAYLAND_SHORTCUTS: &str = "global shortcuts are not available in this Wayland session";
/// The reason a desktop without a `StatusNotifierItem` host gives.
pub const NO_SNI: &str =
    "tray icons need a StatusNotifierItem host, which this desktop session does not run";

/// What the probes look at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Facts {
    /// `macos`, `windows`, `linux`...
    pub os: String,
    /// A native Wayland session (`WAYLAND_DISPLAY` set and the app not
    /// forced onto X11).
    pub wayland: bool,
    /// A `StatusNotifierItem` host (Linux); `None` when it could not be
    /// asked.
    pub sni_host: Option<bool>,
}

impl Facts {
    /// The facts of this machine for an app whose window backend is
    /// forced to X11 when `x11` is set.
    #[must_use]
    pub fn detect(x11: bool) -> Self {
        let os = std::env::consts::OS.to_owned();
        let linux = os == "linux";
        Self {
            wayland: linux && !x11 && std::env::var_os("WAYLAND_DISPLAY").is_some(),
            sni_host: if linux { sni_host() } else { Some(true) },
            os,
        }
    }
}

/// Asks the session bus whether a `StatusNotifierWatcher` exists.
fn sni_host() -> Option<bool> {
    let out = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.DBus",
            "--type=method_call",
            "--print-reply",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            "string:org.kde.StatusNotifierWatcher",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).contains("boolean true"))
}

/// One row, shaped like the daemon's `platform_support` rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Feature {
    /// The feature id.
    pub feature: &'static str,
    /// `supported`, `degraded` or `unsupported`.
    pub state: &'static str,
    /// What the probe found.
    pub reason: String,
}

fn row(feature: &'static str, state: &'static str, reason: impl Into<String>) -> Feature {
    Feature {
        feature,
        state,
        reason: reason.into(),
    }
}

/// The shell's rows for `facts`.
#[must_use]
pub fn probe(facts: &Facts) -> Vec<Feature> {
    let shortcut = if facts.wayland {
        // The shortcut plugin grabs keys through X11; a native Wayland
        // session gives it nothing.
        row(GLOBAL_SHORTCUT, "unsupported", WAYLAND_SHORTCUTS)
    } else {
        row(
            GLOBAL_SHORTCUT,
            "supported",
            "registered with the window system",
        )
    };
    let tray = match facts.sni_host {
        Some(true) => row(TRAY, "supported", "the system tray is available"),
        Some(false) => row(TRAY, "unsupported", NO_SNI),
        None => row(
            TRAY,
            "degraded",
            "could not ask the session bus for a StatusNotifierItem host",
        ),
    };
    let badge = if facts.os == "linux" {
        row(
            BADGE,
            "degraded",
            "only launchers that implement the Unity launcher API show badges",
        )
    } else {
        row(BADGE, "supported", "the dock or taskbar shows badges")
    };
    let (snap, below) = if facts.wayland {
        (
            row(HUD_SNAP, "unsupported", WAYLAND_PLACEMENT),
            row(HUD_BELOW, "unsupported", WAYLAND_WINDOWS),
        )
    } else {
        (
            row(HUD_SNAP, "supported", "the HUD moves to the pointer"),
            row(
                HUD_BELOW,
                "supported",
                "window titles and bounds are readable",
            ),
        )
    };
    vec![shortcut, tray, badge, snap, below]
}

#[cfg(test)]
mod tests {
    use super::{Facts, GLOBAL_SHORTCUT, WAYLAND_SHORTCUTS, probe};

    #[test]
    fn wayland_without_the_portal_disables_the_hotkey_with_the_reason() {
        let facts = Facts {
            os: "linux".into(),
            wayland: true,
            sni_host: Some(false),
        };
        let rows = probe(&facts);
        let hotkey = rows.iter().find(|r| r.feature == GLOBAL_SHORTCUT).unwrap();
        assert_eq!(hotkey.state, "unsupported");
        assert_eq!(hotkey.reason, WAYLAND_SHORTCUTS);
        assert!(
            rows.iter()
                .any(|r| r.feature == "desktop.tray" && r.state == "unsupported")
        );
    }

    /// Scenario: Snap on native Wayland.
    #[test]
    fn native_wayland_disables_hud_placement_with_the_reason() {
        let facts = Facts {
            os: "linux".into(),
            wayland: true,
            sni_host: Some(true),
        };
        let rows = probe(&facts);
        let snap = rows.iter().find(|r| r.feature == super::HUD_SNAP).unwrap();
        assert_eq!(snap.state, "unsupported");
        assert_eq!(snap.reason, "the compositor controls window placement");
    }

    #[test]
    fn macos_supports_every_shell_feature() {
        let facts = Facts {
            os: "macos".into(),
            wayland: false,
            sni_host: Some(true),
        };
        assert!(probe(&facts).iter().all(|r| r.state == "supported"));
    }
}
