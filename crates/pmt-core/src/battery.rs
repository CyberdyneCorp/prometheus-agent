//! The machine's battery for the status bars of the TUI and the line
//! interface (spec: tui, Client-local display commands): its charge and whether it is charging, or `None` on
//! a machine without one (the item is then silently left out). Read from
//! `/sys/class/power_supply` on Linux, `pmset -g batt` on macOS and the
//! `Win32_Battery` class on Windows; reading may take a moment, so callers
//! read off their event loop.

use std::path::Path;

/// One battery reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Charge, 0 to 100.
    pub percent: u8,
    /// On external power and charging.
    pub charging: bool,
}

/// How full a battery looks, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Above half.
    High,
    /// 20 to 50 %.
    Medium,
    /// Below 20 %.
    Low,
}

impl Battery {
    /// Its colour band.
    #[must_use]
    pub const fn level(self) -> Level {
        match self.percent {
            51.. => Level::High,
            20..=50 => Level::Medium,
            _ => Level::Low,
        }
    }

    /// The status item: `▮ 85%`, with `+` while charging.
    #[must_use]
    pub fn item(self) -> String {
        let charging = if self.charging { "+" } else { "" };
        format!("▮ {}%{charging}", self.percent)
    }

    /// `/battery status`.
    #[must_use]
    pub fn describe(self) -> String {
        let state = if self.charging {
            "charging"
        } else {
            "on battery"
        };
        format!("battery: {}% ({state})", self.percent)
    }
}

/// What `/battery status` prints without a battery.
pub const NONE: &str = "no battery detected";

/// The battery a test seam describes: `none`, or `<percent>[,charging]`.
/// `None` for a value that is neither.
#[must_use]
pub fn from_seam(value: &str) -> Option<Option<Battery>> {
    if value == "none" {
        return Some(None);
    }
    let (percent, charging) = match value.split_once(',') {
        Some((percent, "charging")) => (percent, true),
        Some(_) => return None,
        None => (value, false),
    };
    let percent = percent.parse::<u8>().ok().filter(|p| *p <= 100)?;
    Some(Some(Battery { percent, charging }))
}

/// This machine's battery.
#[must_use]
pub fn read() -> Option<Battery> {
    if cfg!(target_os = "macos") {
        let out = command_output("pmset", &["-g", "batt"])?;
        parse_pmset(&out)
    } else if cfg!(windows) {
        let script = "$b = Get-CimInstance Win32_Battery | Select-Object -First 1; \
                      if ($b) { \"$($b.EstimatedChargeRemaining) $($b.BatteryStatus)\" }";
        let out = command_output("powershell", &["-NoProfile", "-Command", script])?;
        parse_win32(&out)
    } else {
        read_sysfs(Path::new("/sys/class/power_supply"))
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `pmset -g batt`: a desktop Mac prints no `InternalBattery` line.
fn parse_pmset(text: &str) -> Option<Battery> {
    let line = text.lines().find(|l| l.contains("InternalBattery"))?;
    let percent = line
        .split(['\t', ' ', ';'])
        .find_map(|word| word.strip_suffix('%'))?
        .parse::<u8>()
        .ok()?;
    let charging = line.contains("; charging") || line.contains("; charged");
    Some(Battery {
        percent: percent.min(100),
        charging,
    })
}

/// `<EstimatedChargeRemaining> <BatteryStatus>`; status 2 and 6..=9 are
/// on external power.
fn parse_win32(text: &str) -> Option<Battery> {
    let mut words = text.split_whitespace();
    let percent = words.next()?.parse::<u8>().ok()?;
    let status = words.next().and_then(|s| s.parse::<u8>().ok()).unwrap_or(1);
    Some(Battery {
        percent: percent.min(100),
        charging: matches!(status, 2 | 6..=9),
    })
}

/// The first `type = Battery` supply under `root` that reports a capacity.
fn read_sysfs(root: &Path) -> Option<Battery> {
    let mut supplies: Vec<_> = std::fs::read_dir(root).ok()?.flatten().collect();
    supplies.sort_by_key(std::fs::DirEntry::file_name);
    supplies.into_iter().find_map(|entry| {
        let dir = entry.path();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        if read("type")?.trim() != "Battery" {
            return None;
        }
        let percent = read("capacity")?.trim().parse::<u8>().ok()?;
        let status = read("status").unwrap_or_default();
        Some(Battery {
            percent: percent.min(100),
            charging: matches!(status.trim(), "Charging" | "Full"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{Battery, Level, from_seam, parse_pmset, parse_win32, read_sysfs};

    #[test]
    fn pmset_reports_a_laptop_battery_and_nothing_on_a_desktop() {
        let laptop = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t83%; discharging; 5:21 remaining present: true\n";
        assert_eq!(
            parse_pmset(laptop),
            Some(Battery {
                percent: 83,
                charging: false
            })
        );
        let plugged = " -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true";
        assert!(parse_pmset(plugged).unwrap().charging);
        assert_eq!(parse_pmset("Now drawing from 'AC Power'\n"), None);
    }

    #[test]
    fn sysfs_finds_the_battery_among_the_supplies() {
        let root = tempfile::tempdir().unwrap();
        let ac = root.path().join("AC");
        std::fs::create_dir(&ac).unwrap();
        std::fs::write(ac.join("type"), "Mains\n").unwrap();
        assert_eq!(read_sysfs(root.path()), None, "no battery");
        let bat = root.path().join("BAT0");
        std::fs::create_dir(&bat).unwrap();
        std::fs::write(bat.join("type"), "Battery\n").unwrap();
        std::fs::write(bat.join("capacity"), "17\n").unwrap();
        std::fs::write(bat.join("status"), "Charging\n").unwrap();
        let found = read_sysfs(root.path()).unwrap();
        assert_eq!((found.percent, found.charging), (17, true));
        assert_eq!(found.level(), Level::Low);
    }

    #[test]
    fn windows_and_the_test_seam_parse() {
        assert_eq!(
            parse_win32("64 2\r\n"),
            Some(Battery {
                percent: 64,
                charging: true
            })
        );
        assert_eq!(parse_win32(""), None);
        assert_eq!(from_seam("none"), Some(None));
        assert_eq!(
            from_seam("40,charging"),
            Some(Some(Battery {
                percent: 40,
                charging: true
            }))
        );
        assert_eq!(from_seam("140"), None);
        let half = Battery {
            percent: 50,
            charging: false,
        };
        assert_eq!(half.level(), Level::Medium);
        assert_eq!(half.item(), "▮ 50%");
        assert_eq!(half.describe(), "battery: 50% (on battery)");
    }
}
