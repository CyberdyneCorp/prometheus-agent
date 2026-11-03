//! Hermes gateway launchers on Windows (spec: messaging-gateway, Per-OS
//! service installation without elevation). Hermes installs the Scheduled
//! Task `Hermes_Gateway` (`Hermes_Gateway_<suffix>` for a named profile) at
//! the Task Scheduler root, or, when Task Scheduler refuses, the Startup
//! folder entry `Hermes_Gateway[_<suffix>].vbs` / `.cmd`. Each is reported
//! with the cmd.exe command that removes it and installs the Prometheus
//! service in its place.

use std::path::Path;

use super::legacy::{HERMES, LegacyUnit, Roots, under};

const BASE: &str = "Hermes_Gateway";
const STARTUP: &[&str] = &[
    "AppData",
    "Roaming",
    "Microsoft",
    "Windows",
    "Start Menu",
    "Programs",
    "Startup",
];

/// `name` is `Hermes_Gateway` or `Hermes_Gateway_<suffix>`.
fn is_hermes(name: &str) -> bool {
    name.strip_prefix(BASE).is_some_and(|rest| {
        rest.is_empty() || rest.strip_prefix('_').is_some_and(|s| !s.is_empty())
    })
}

/// Task names from `schtasks /Query /FO CSV /NH`: the first quoted column
/// of each row (a task with several triggers may repeat).
pub(super) fn task_names(csv: &str) -> Vec<String> {
    let mut names: Vec<String> = csv
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"')?.split('"').next())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// cmd.exe double quoting (`"` cannot occur in a Windows path or task name).
fn quote(text: &str) -> String {
    format!("\"{text}\"")
}

fn install(home: Option<&Path>) -> String {
    let home = home.map_or_else(String::new, |h| {
        format!(" --home {}", quote(&h.to_string_lossy()))
    });
    format!("prometheus{home} daemon install-service")
}

/// Scheduled Tasks first, then Startup entries, each sorted.
pub(super) fn units(roots: &Roots<'_>) -> Vec<LegacyUnit> {
    let install = install(roots.home);
    let mut found: Vec<LegacyUnit> = roots
        .tasks
        .iter()
        // Hermes creates its task at the root folder (`\Hermes_Gateway`).
        .filter_map(|task| task.strip_prefix('\\').filter(|n| is_hermes(n)))
        .map(|name| LegacyUnit {
            location: format!("Scheduled Task \\{name}"),
            origin: HERMES,
            // `/End` fails for a task that is not running, which is fine:
            // `&` runs the delete regardless, `&&` only after it succeeds.
            replace: format!(
                "schtasks /End /TN {task} & schtasks /Delete /TN {task} /F && {install}",
                task = quote(name)
            ),
        })
        .collect();
    let startup = under(roots.user_home, STARTUP);
    let Ok(entries) = std::fs::read_dir(&startup) else {
        return found;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let launcher = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("vbs") || e.eq_ignore_ascii_case("cmd"));
        let stem = path.file_stem().and_then(|s| s.to_str());
        if launcher && stem.is_some_and(is_hermes) {
            found.push(LegacyUnit {
                location: path.display().to_string(),
                origin: HERMES,
                replace: format!("del {} && {install}", quote(&path.to_string_lossy())),
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schtasks_csv_rows_yield_each_task_once() {
        let csv = "\"\\Hermes_Gateway\",\"N/A\",\"Ready\"\r\n\
                   \"\\Hermes_Gateway\",\"11/2/2026 9:00:00 AM\",\"Ready\"\r\n\
                   \"\\Microsoft\\Windows\\Defrag\\ScheduledDefrag\",\"N/A\",\"Ready\"\r\n\
                   \r\n";
        assert_eq!(
            task_names(csv),
            [
                "\\Hermes_Gateway",
                "\\Microsoft\\Windows\\Defrag\\ScheduledDefrag"
            ]
        );
    }

    #[test]
    fn hermes_tasks_and_startup_launchers_are_found_with_their_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("home");
        let startup = under(&user, STARTUP);
        std::fs::create_dir_all(&startup).unwrap();
        for name in [
            "Hermes_Gateway.vbs",
            "Hermes_Gateway_work.cmd",
            "Hermes_Gateway.tmp",
            "Hermes_Gatewayx.vbs",
            "Prometheus Daemon.cmd",
        ] {
            std::fs::write(startup.join(name), "").unwrap();
        }
        let tasks = [
            "\\Hermes_Gateway_coder".to_owned(),
            "\\Hermes_Gateway_".to_owned(),
            "\\Other\\Hermes_Gateway".to_owned(),
            "\\Prometheus Daemon".to_owned(),
        ];
        let home = dir.path().join("work home");
        let found = units(&Roots {
            user_home: &user,
            system_root: dir.path(),
            home: Some(&home),
            tasks: &tasks,
        });
        let install = format!(
            "prometheus --home \"{}\" daemon install-service",
            home.display()
        );
        let vbs = startup.join("Hermes_Gateway.vbs");
        let cmd = startup.join("Hermes_Gateway_work.cmd");
        let expected = [
            (
                "Scheduled Task \\Hermes_Gateway_coder".to_owned(),
                format!(
                    "schtasks /End /TN \"Hermes_Gateway_coder\" & \
                     schtasks /Delete /TN \"Hermes_Gateway_coder\" /F && {install}"
                ),
            ),
            (
                vbs.display().to_string(),
                format!("del \"{}\" && {install}", vbs.display()),
            ),
            (
                cmd.display().to_string(),
                format!("del \"{}\" && {install}", cmd.display()),
            ),
        ];
        let got: Vec<_> = found
            .iter()
            .map(|u| (u.location.clone(), u.replace.clone()))
            .collect();
        assert_eq!(got, expected);
        assert!(found.iter().all(|u| u.origin == HERMES));
    }
}
