//! Supervisor units left by an earlier Prometheus layout or an imported
//! Hermes gateway (spec: messaging-gateway, Per-OS service installation
//! without elevation). `prometheus doctor` reports each with the exact
//! command that replaces it; nothing here removes a unit.
//!
//! Hermes names its units per profile (`hermes-gateway.service` for the
//! default home, `hermes-gateway-<profile>.service` otherwise; the same
//! with `ai.hermes.gateway[-<suffix>]` on macOS), in the user scope and in
//! the system scope, so every scope is searched for the whole family.

use std::path::{Path, PathBuf};

/// A legacy unit and the command that replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyUnit {
    /// Where it is.
    pub path: PathBuf,
    /// What installed it.
    pub origin: &'static str,
    /// The shell command that stops and removes it and installs the
    /// Prometheus service in its place.
    pub replace: String,
}

#[derive(Clone, Copy)]
enum Manager {
    Systemd,
    Launchd,
}

/// One family of unit files: `<base><ext>` or `<base>-<suffix><ext>` in
/// `dir` (relative to the user's home, or to the system root).
struct Family {
    dir: &'static str,
    system: bool,
    manager: Manager,
    base: &'static str,
    origin: &'static str,
}

const HERMES: &str = "Hermes gateway";
const EARLIER: &str = "an earlier Prometheus layout";

const FAMILIES: &[Family] = &[
    Family::systemd(".config/systemd/user", false, "hermes-gateway", HERMES),
    Family::systemd(".config/systemd/user", false, "prometheus-gateway", EARLIER),
    Family::systemd("etc/systemd/system", true, "hermes-gateway", HERMES),
    Family::systemd("etc/systemd/system", true, "prometheus-gateway", EARLIER),
    Family::launchd("Library/LaunchAgents", false, "ai.hermes.gateway", HERMES),
    Family::launchd(
        "Library/LaunchAgents",
        false,
        "ai.prometheus.gateway",
        EARLIER,
    ),
    Family::launchd("Library/LaunchDaemons", true, "ai.hermes.gateway", HERMES),
    Family::launchd(
        "Library/LaunchDaemons",
        true,
        "ai.prometheus.gateway",
        EARLIER,
    ),
];

impl Family {
    const fn systemd(
        dir: &'static str,
        system: bool,
        base: &'static str,
        origin: &'static str,
    ) -> Self {
        Self {
            dir,
            system,
            manager: Manager::Systemd,
            base,
            origin,
        }
    }

    const fn launchd(
        dir: &'static str,
        system: bool,
        base: &'static str,
        origin: &'static str,
    ) -> Self {
        Self {
            dir,
            system,
            manager: Manager::Launchd,
            base,
            origin,
        }
    }

    fn extension(&self) -> &'static str {
        match self.manager {
            Manager::Systemd => ".service",
            Manager::Launchd => ".plist",
        }
    }

    /// The unit name (file name without the extension) when `file` belongs
    /// to this family.
    fn unit_name<'a>(&self, file: &'a str) -> Option<&'a str> {
        let name = file.strip_suffix(self.extension())?;
        let rest = name.strip_prefix(self.base)?;
        (rest.is_empty() || rest.strip_prefix('-').is_some_and(|s| !s.is_empty())).then_some(name)
    }
}

/// Where the units are searched.
pub(super) struct Roots<'a> {
    /// The user's home directory.
    pub(super) user_home: &'a Path,
    /// Prefix of system paths (`/` for real).
    pub(super) system_root: &'a Path,
    /// The Prometheus home when it is not the default one (the replacing
    /// install must name it).
    pub(super) home: Option<&'a Path>,
}

/// Every legacy supervisor unit, user scope first, sorted by path.
#[must_use]
pub(super) fn legacy_units(roots: &Roots<'_>) -> Vec<LegacyUnit> {
    let mut found = Vec::new();
    for family in FAMILIES {
        let base = if family.system {
            roots.system_root
        } else {
            roots.user_home
        };
        let dir = base.join(family.dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            if let Some(name) = family.unit_name(file) {
                let replace = replace_command(family, name, &path, roots.home);
                found.push(LegacyUnit {
                    origin: family.origin,
                    replace,
                    path,
                });
            }
        }
    }
    found
}

/// POSIX single quoting (paths may hold spaces).
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn replace_command(family: &Family, name: &str, path: &Path, home: Option<&Path>) -> String {
    let file = quote(&path.to_string_lossy());
    let sudo = if family.system { "sudo " } else { "" };
    let stop = match (family.manager, family.system) {
        (Manager::Systemd, false) => format!(
            "systemctl --user disable --now {name}.service && rm {file} && systemctl --user daemon-reload"
        ),
        (Manager::Systemd, true) => format!(
            "sudo systemctl disable --now {name}.service && sudo rm {file} && sudo systemctl daemon-reload"
        ),
        // bootout fails for a unit that is not loaded, which is fine.
        (Manager::Launchd, false) => format!("launchctl bootout gui/$(id -u)/{name}; rm {file}"),
        (Manager::Launchd, true) => format!("sudo launchctl bootout system/{name}; sudo rm {file}"),
    };
    let home = home.map_or_else(String::new, |h| {
        format!(" --home {}", quote(&h.to_string_lossy()))
    });
    let scope = if family.system {
        let user = run_as_user(family.manager, path).unwrap_or_else(|| "root".to_owned());
        format!(" --system --run-as-user {}", quote(&user))
    } else {
        String::new()
    };
    format!("{stop} && {sudo}prometheus{home} daemon install-service{scope}")
}

/// The user a system unit runs as (`User=` / `UserName`), so the
/// replacement runs the daemon as the same account.
fn run_as_user(manager: Manager, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    match manager {
        Manager::Systemd => text
            .lines()
            .find_map(|l| l.trim().strip_prefix("User="))
            .map(|u| u.trim().to_owned()),
        Manager::Launchd => {
            let after = text.split("<key>UserName</key>").nth(1)?;
            let value = after.trim_start().strip_prefix("<string>")?;
            value.split("</string>").next().map(|u| u.trim().to_owned())
        }
    }
    .filter(|u| !u.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn hermes_profile_and_system_units_are_found_with_their_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("home");
        let system = dir.path().join("system");
        let roots = Roots {
            user_home: &user,
            system_root: &system,
            home: None,
        };
        assert!(legacy_units(&roots).is_empty());
        put(
            &user.join(".config/systemd/user/hermes-gateway-work.service"),
            "[Unit]\n",
        );
        put(
            &user.join(".config/systemd/user/hermes-gateway-ticket.json"),
            "{}",
        );
        put(
            &user.join(".config/systemd/user/hermes-gatewayx.service"),
            "",
        );
        put(
            &system.join("etc/systemd/system/hermes-gateway.service"),
            "[Service]\nUser=alice\n",
        );
        put(
            &user.join("Library/LaunchAgents/ai.hermes.gateway-coder.plist"),
            "",
        );
        let found = legacy_units(&roots);
        let names: Vec<_> = found
            .iter()
            .map(|u| u.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "hermes-gateway-work.service",
                "hermes-gateway.service",
                "ai.hermes.gateway-coder.plist"
            ]
        );
        assert!(found.iter().all(|u| u.origin == HERMES));
        let user_unit = user.join(".config/systemd/user/hermes-gateway-work.service");
        assert_eq!(
            found[0].replace,
            format!(
                "systemctl --user disable --now hermes-gateway-work.service && rm '{}' && \
                 systemctl --user daemon-reload && prometheus daemon install-service",
                user_unit.display()
            )
        );
        assert!(
            found[1]
                .replace
                .starts_with("sudo systemctl disable --now hermes-gateway.service"),
            "{}",
            found[1].replace
        );
        assert!(
            found[1]
                .replace
                .ends_with("sudo prometheus daemon install-service --system --run-as-user 'alice'"),
            "{}",
            found[1].replace
        );
        assert!(
            found[2]
                .replace
                .starts_with("launchctl bootout gui/$(id -u)/ai.hermes.gateway-coder; rm "),
            "{}",
            found[2].replace
        );
    }

    #[test]
    fn a_custom_home_is_named_in_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("work home");
        put(
            &dir.path()
                .join("system/Library/LaunchDaemons/ai.prometheus.gateway.plist"),
            "<dict>\n<key>UserName</key>\n<string>bob</string>\n</dict>",
        );
        let found = legacy_units(&Roots {
            user_home: &dir.path().join("home"),
            system_root: &dir.path().join("system"),
            home: Some(&home),
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin, EARLIER);
        assert!(
            found[0].replace.ends_with(&format!(
                "&& sudo prometheus --home '{}' daemon install-service --system --run-as-user 'bob'",
                home.display()
            )),
            "{}",
            found[0].replace
        );
    }
}
