//! The supervisor units `daemon install-service` writes, one generator per
//! OS (pure text, so every OS's unit is tested on every host). Each unit
//! runs `prometheus daemon run`, restarts it after exit 75 (restart, self-
//! update) or 1 (fatal error) and never after 0 (requested stop) or 78
//! (configuration error, which would only crash-loop).

use std::fmt::Write as _;
use std::path::Path;

/// The systemd unit name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const SYSTEMD_UNIT: &str = "prometheus-daemon.service";
/// The launchd label.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const LAUNCHD_LABEL: &str = "ai.prometheus.daemon";
/// The Windows Scheduled Task name.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TASK_NAME: &str = "Prometheus Daemon";

/// What every unit runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Daemon<'a> {
    /// The `prometheus` binary.
    pub(crate) exe: &'a Path,
    /// The home, when it is not the default one.
    pub(crate) home: Option<&'a Path>,
    /// The user a system-scope unit runs as.
    pub(crate) run_as: Option<&'a str>,
    /// `daemon.systemd_watchdog_secs`: `> 0` makes the systemd unit
    /// `Type=notify` with this `WatchdogSec`.
    pub(crate) watchdog_secs: u64,
}

/// Quotes a path for systemd's `ExecStart` (C-style double quotes).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A systemd unit: `--user` (wanted by `default.target`) or system-wide
/// (`multi-user.target`, `User=`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn systemd(daemon: &Daemon<'_>) -> String {
    let mut unit = String::from(
        "[Unit]\nDescription=Prometheus agent daemon\nAfter=network-online.target\n\
         Wants=network-online.target\n\n[Service]\n",
    );
    if daemon.watchdog_secs > 0 {
        // The daemon notifies only while its liveness probes succeed.
        let _ = writeln!(
            unit,
            "Type=notify\nNotifyAccess=main\nWatchdogSec={}",
            daemon.watchdog_secs
        );
    } else {
        unit.push_str("Type=simple\n");
    }
    let _ = writeln!(
        unit,
        "ExecStart={} daemon run",
        systemd_quote(&daemon.exe.to_string_lossy())
    );
    if let Some(home) = daemon.home {
        let _ = writeln!(
            unit,
            "Environment={}",
            systemd_quote(&format!("PROMETHEUS_HOME={}", home.to_string_lossy()))
        );
    }
    if let Some(user) = daemon.run_as {
        let _ = writeln!(unit, "User={user}");
    }
    unit.push_str(
        "# 75 (restart, update) and 1 (fatal) restart; 0 (stop) and 78 (configuration) do not.\n\
         Restart=on-failure\nRestartSec=5\nRestartPreventExitStatus=78\n\n[Install]\n",
    );
    let target = if daemon.run_as.is_some() {
        "multi-user.target"
    } else {
        "default.target"
    };
    let _ = writeln!(unit, "WantedBy={target}");
    unit
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A launchd property list: a `LaunchAgent`, or with `run_as` a
/// `LaunchDaemon`. launchd cannot exclude one exit status from `KeepAlive`,
/// so a small shell wrapper turns 78 into 0 (not restarted).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn launchd(daemon: &Daemon<'_>, stderr: &Path) -> String {
    let exe = xml_escape(&daemon.exe.to_string_lossy());
    let mut plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>/bin/sh</string>\n\t\t<string>-c</string>\n\
         \t\t<string>\"$0\" daemon run; s=$?; [ \"$s\" -eq 78 ] &amp;&amp; exit 0; exit \"$s\"</string>\n\
         \t\t<string>{exe}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n",
        xml_escape(&stderr.to_string_lossy())
    );
    if let Some(home) = daemon.home {
        let _ = write!(
            plist,
            "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PROMETHEUS_HOME</key>\n\t\t<string>{}</string>\n\t</dict>\n",
            xml_escape(&home.to_string_lossy())
        );
    }
    if let Some(user) = daemon.run_as {
        let _ = write!(
            plist,
            "\t<key>UserName</key>\n\t<string>{}</string>\n",
            xml_escape(user)
        );
    }
    plist.push_str("</dict>\n</plist>\n");
    plist
}

/// The Windows supervisor script the Scheduled Task starts: it runs the
/// daemon and restarts it after 75 or 1, not after 0 or 78.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_wrapper(daemon: &Daemon<'_>) -> String {
    let mut script =
        String::from("@echo off\r\nrem Written by 'prometheus daemon install-service'.\r\n");
    if let Some(home) = daemon.home {
        let _ = write!(
            script,
            "set \"PROMETHEUS_HOME={}\"\r\n",
            home.to_string_lossy()
        );
    }
    let _ = write!(
        script,
        ":run\r\n\"{}\" daemon run\r\n\
         if %ERRORLEVEL% EQU 0 exit /b 0\r\n\
         if %ERRORLEVEL% EQU 78 exit /b 78\r\n\
         timeout /t 5 /nobreak >nul\r\ngoto run\r\n",
        daemon.exe.to_string_lossy()
    );
    script
}

/// The Startup-folder launcher used when Task Scheduler refuses: it runs
/// `prometheus daemon start` at logon.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_startup(daemon: &Daemon<'_>) -> String {
    let mut script = String::from(
        "@echo off\r\nrem Written by 'prometheus daemon install-service' (Task Scheduler refused).\r\n",
    );
    if let Some(home) = daemon.home {
        let _ = write!(
            script,
            "set \"PROMETHEUS_HOME={}\"\r\n",
            home.to_string_lossy()
        );
    }
    let _ = write!(
        script,
        "start \"\" /b \"{}\" daemon start\r\n",
        daemon.exe.to_string_lossy()
    );
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/prometheus/bin/prometheus";

    fn daemon<'a>(home: Option<&'a Path>, run_as: Option<&'a str>) -> Daemon<'a> {
        Daemon {
            exe: Path::new(EXE),
            home,
            run_as,
            watchdog_secs: 0,
        }
    }

    #[test]
    fn a_systemd_watchdog_makes_the_unit_notify() {
        assert!(systemd(&daemon(None, None)).contains("Type=simple\n"));
        let unit = systemd(&Daemon {
            watchdog_secs: 90,
            ..daemon(None, None)
        });
        assert!(
            unit.contains("Type=notify\nNotifyAccess=main\nWatchdogSec=90\n"),
            "{unit}"
        );
        assert!(!unit.contains("Type=simple"));
    }

    #[test]
    fn the_systemd_user_unit_restarts_on_75_and_1_but_not_78() {
        let unit = systemd(&daemon(Some(Path::new("/srv/pmt work")), None));
        assert!(unit.contains("ExecStart=\"/opt/prometheus/bin/prometheus\" daemon run\n"));
        assert!(unit.contains("Environment=\"PROMETHEUS_HOME=/srv/pmt work\"\n"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(unit.contains("RestartPreventExitStatus=78\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
        assert!(!unit.contains("User="));
    }

    #[test]
    fn the_systemd_system_unit_runs_as_the_user() {
        let unit = systemd(&daemon(None, Some("alice")));
        assert!(unit.contains("User=alice\n"));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
        assert!(!unit.contains("PROMETHEUS_HOME"));
    }

    #[test]
    fn the_launch_agent_turns_78_into_a_clean_exit() {
        let plist = launchd(
            &daemon(Some(Path::new("/Users/a/.pmt")), None),
            Path::new("/Users/a/.pmt/logs/daemon-stderr.log"),
        );
        assert!(plist.contains("<string>ai.prometheus.daemon</string>"));
        assert!(plist.contains("[ \"$s\" -eq 78 ] &amp;&amp; exit 0"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(plist.contains(&format!("<string>{EXE}</string>")));
        assert!(plist.contains("<key>PROMETHEUS_HOME</key>\n\t\t<string>/Users/a/.pmt</string>"));
        assert!(!plist.contains("UserName"));
        let system = launchd(&daemon(None, Some("alice")), Path::new("/var/log/p.log"));
        assert!(system.contains("<key>UserName</key>\n\t<string>alice</string>"));
    }

    #[test]
    fn the_windows_wrapper_supervises_and_the_fallback_starts() {
        let wrapper = windows_wrapper(&daemon(Some(Path::new(r"D:\pmt")), None));
        assert!(wrapper.contains("set \"PROMETHEUS_HOME=D:\\pmt\"\r\n"));
        assert!(wrapper.contains("if %ERRORLEVEL% EQU 78 exit /b 78\r\n"));
        assert!(wrapper.contains("goto run\r\n"));
        let startup = windows_startup(&daemon(None, None));
        assert!(startup.contains("daemon start"));
        assert!(!startup.contains("daemon run"));
    }
}
