//! HUD mode (spec: desktop-app, HUD mode): the window underneath the
//! floating bar, described by metadata only (application, title, bounds;
//! never pixels), which a turn submitted from the HUD carries as context.
//!
//! Each OS lists its windows front to back with what the shell can run
//! without native bindings: `CoreGraphics` through JavaScript for
//! Automation on macOS, `EnumWindows` through `PowerShell` on Windows,
//! `xprop`/`xwininfo` on X11. A native Wayland session lists nothing (the
//! compositor does not tell applications about other windows). The
//! topmost ordinary window of another process overlapping the bar wins.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::platform::Facts;

/// A rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Bounds {
    /// Left.
    pub x: i64,
    /// Top.
    pub y: i64,
    /// Width.
    pub width: i64,
    /// Height.
    pub height: i64,
}

impl Bounds {
    fn overlaps(&self, other: &Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// The window underneath the HUD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBelow {
    /// The application.
    pub app: String,
    /// The window's title.
    pub title: String,
    /// Where it is.
    pub bounds: Bounds,
}

/// One window of the screen, as an OS lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Candidate {
    /// The application.
    pub app: String,
    /// The window's title.
    #[serde(default)]
    pub title: String,
    /// Its owner process.
    #[serde(default)]
    pub pid: u32,
    /// Its layer (0: ordinary windows).
    #[serde(default)]
    pub layer: i64,
    /// Where it is.
    pub bounds: Bounds,
}

/// The topmost ordinary window of another process overlapping `hud`;
/// `windows` are front to back.
#[must_use]
pub fn pick(windows: &[Candidate], hud: &Bounds, own_pid: u32) -> Option<WindowBelow> {
    windows
        .iter()
        .find(|w| w.pid != own_pid && w.layer == 0 && w.bounds.width > 0 && w.bounds.overlaps(hud))
        .map(|w| WindowBelow {
            app: w.app.clone(),
            title: w.title.clone(),
            bounds: w.bounds,
        })
}

/// The window underneath `hud` on this machine (`None`: none, or the
/// platform does not say).
#[must_use]
pub fn below(facts: &Facts, hud: &Bounds) -> Option<WindowBelow> {
    let windows = match facts.os.as_str() {
        "macos" => json_lines(&run(
            "osascript",
            &["-l", "JavaScript", "-e", MACOS_SCRIPT],
        )?),
        "windows" => json_lines(&run(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", WINDOWS_SCRIPT],
        )?),
        "linux" if !facts.wayland => x11_windows()?,
        _ => return None,
    };
    pick(&windows, hud, std::process::id())
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One JSON window per line.
#[must_use]
pub fn json_lines(text: &str) -> Vec<Candidate> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

/// macOS: on-screen windows front to back (titles need the Screen
/// Recording permission; without it they are empty).
const MACOS_SCRIPT: &str = r"
ObjC.import('CoreGraphics');
var list = ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly | $.kCGWindowListExcludeDesktopElements, 0));
var out = [];
for (var i = 0; i < list.count; i++) {
  var w = ObjC.deepUnwrap(list.objectAtIndex(i));
  var b = w.kCGWindowBounds || {};
  out.push(JSON.stringify({app: w.kCGWindowOwnerName || '', title: w.kCGWindowName || '', pid: w.kCGWindowOwnerPID || 0,
    layer: w.kCGWindowLayer || 0, bounds: {x: Math.round(b.X || 0), y: Math.round(b.Y || 0), width: Math.round(b.Width || 0), height: Math.round(b.Height || 0)}}));
}
out.join('\n');
";

/// Windows: visible top-level windows front to back.
const WINDOWS_SCRIPT: &str = r#"
Add-Type @'
using System; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public static class PmtWindows {
  public delegate bool Cb(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Cb cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public struct RECT { public int L, T, R, B; }
  public static List<IntPtr> All() { var l = new List<IntPtr>(); EnumWindows((h, p) => { if (IsWindowVisible(h)) l.Add(h); return true; }, IntPtr.Zero); return l; }
}
'@
foreach ($h in [PmtWindows]::All()) {
  $s = New-Object System.Text.StringBuilder 512; [void][PmtWindows]::GetWindowText($h, $s, 512)
  $r = New-Object PmtWindows+RECT; [void][PmtWindows]::GetWindowRect($h, [ref]$r)
  $p = 0; [void][PmtWindows]::GetWindowThreadProcessId($h, [ref]$p)
  $app = (Get-Process -Id $p -ErrorAction SilentlyContinue).ProcessName
  @{app = "$app"; title = $s.ToString(); pid = [int]$p; layer = 0; bounds = @{x = $r.L; y = $r.T; width = $r.R - $r.L; height = $r.B - $r.T}} | ConvertTo-Json -Compress
}
"#;

/// X11: the stacking order of the window manager, then each window's
/// name, class, process and geometry.
fn x11_windows() -> Option<Vec<Candidate>> {
    let stacking = run("xprop", &["-root", "_NET_CLIENT_LIST_STACKING"])?;
    let mut windows: Vec<Candidate> = stacking_ids(&stacking)
        .iter()
        .rev()
        .filter_map(|id| {
            let props = run(
                "xprop",
                &["-id", id, "_NET_WM_NAME", "WM_CLASS", "_NET_WM_PID"],
            )?;
            let geometry = run("xwininfo", &["-id", id])?;
            Some(x11_candidate(&props, &geometry))
        })
        .collect();
    windows.retain(|w| w.bounds.width > 0);
    Some(windows)
}

/// The window ids of `xprop -root _NET_CLIENT_LIST_STACKING` (bottom to
/// top).
#[must_use]
pub fn stacking_ids(text: &str) -> Vec<String> {
    text.split_once('#')
        .map(|(_, ids)| {
            ids.split(',')
                .map(|id| id.trim().to_owned())
                .filter(|id| id.starts_with("0x"))
                .collect()
        })
        .unwrap_or_default()
}

/// A window from its `xprop` properties and `xwininfo` geometry.
#[must_use]
pub fn x11_candidate(props: &str, geometry: &str) -> Candidate {
    let quoted = |key: &str| {
        props
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split('"').nth(if key == "WM_CLASS" { 3 } else { 1 }))
            .unwrap_or_default()
            .to_owned()
    };
    let number = |label: &str| {
        geometry
            .lines()
            .find_map(|l| l.trim().strip_prefix(label))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
    };
    let pid = props
        .lines()
        .find(|l| l.starts_with("_NET_WM_PID"))
        .and_then(|l| l.rsplit('=').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    Candidate {
        app: quoted("WM_CLASS"),
        title: quoted("_NET_WM_NAME"),
        pid,
        layer: 0,
        bounds: Bounds {
            x: number("Absolute upper-left X:"),
            y: number("Absolute upper-left Y:"),
            width: number("Width:"),
            height: number("Height:"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Bounds, json_lines, pick, stacking_ids, x11_candidate};

    const HUD: Bounds = Bounds {
        x: 100,
        y: 100,
        width: 400,
        height: 80,
    };

    /// Scenario: Asking about the app underneath (the pick).
    #[test]
    fn the_topmost_other_window_under_the_bar_is_picked() {
        let windows = json_lines(concat!(
            r#"{"app":"Prometheus","title":"HUD","pid":7,"layer":0,"bounds":{"x":100,"y":100,"width":400,"height":80}}"#,
            "\n",
            r#"{"app":"Dock","title":"","pid":9,"layer":20,"bounds":{"x":0,"y":0,"width":2000,"height":2000}}"#,
            "\n",
            r#"{"app":"Notes","title":"far away","pid":3,"layer":0,"bounds":{"x":900,"y":900,"width":100,"height":100}}"#,
            "\n",
            r#"{"app":"Firefox","title":"Invoice 42","pid":4,"layer":0,"bounds":{"x":0,"y":0,"width":1280,"height":800}}"#,
        ));
        let below = pick(&windows, &HUD, 7).unwrap();
        assert_eq!(
            (below.app.as_str(), below.title.as_str()),
            ("Firefox", "Invoice 42")
        );
        assert!(pick(&windows[..3], &HUD, 7).is_none());
    }

    #[test]
    fn x11_properties_and_geometry_read_into_a_window() {
        let ids =
            stacking_ids("_NET_CLIENT_LIST_STACKING(WINDOW): window id # 0x1a00003, 0x2c00007");
        assert_eq!(ids, ["0x1a00003", "0x2c00007"]);
        let props = "_NET_WM_NAME(UTF8_STRING) = \"Invoice 42 - Mozilla Firefox\"\nWM_CLASS(STRING) = \"Navigator\", \"firefox\"\n_NET_WM_PID(CARDINAL) = 4242\n";
        let geometry = "  Absolute upper-left X:  10\n  Absolute upper-left Y:  20\n  Width: 1280\n  Height: 800\n";
        let window = x11_candidate(props, geometry);
        assert_eq!(window.app, "firefox");
        assert_eq!(window.title, "Invoice 42 - Mozilla Firefox");
        assert_eq!(window.pid, 4242);
        assert_eq!(window.bounds.width, 1280);
        assert_eq!(window.bounds.y, 20);
    }
}
