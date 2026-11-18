//! `prometheus computer-use screen install [-y]` (spec:
//! execution-environments, Bot screen): installs Xvnc and a minimal Xfce
//! session with the distribution's package manager, only after showing the
//! exact package list; without root and without `sudo` it prints the
//! command instead.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One supported package manager and what it installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The package manager.
    pub manager: &'static str,
    /// Its install arguments before the packages.
    pub args: &'static [&'static str],
    /// The exact packages.
    pub packages: &'static [&'static str],
}

/// The package managers known, in lookup order.
const PLANS: &[Plan] = &[
    Plan {
        manager: "apt-get",
        args: &["install", "-y", "--no-install-recommends"],
        packages: &[
            "tigervnc-standalone-server",
            "xfce4-session",
            "xfwm4",
            "xfce4-panel",
            "xfdesktop4",
            "dbus-x11",
        ],
    },
    Plan {
        manager: "dnf",
        args: &["install", "-y"],
        packages: &[
            "tigervnc-server",
            "xfce4-session",
            "xfwm4",
            "xfce4-panel",
            "xfdesktop",
            "dbus-x11",
        ],
    },
    Plan {
        manager: "pacman",
        args: &["-S", "--needed", "--noconfirm"],
        packages: &[
            "tigervnc",
            "xfce4-session",
            "xfwm4",
            "xfce4-panel",
            "xfdesktop",
        ],
    },
    Plan {
        manager: "zypper",
        args: &["--non-interactive", "install"],
        packages: &[
            "xorg-x11-Xvnc",
            "xfce4-session",
            "xfwm4",
            "xfce4-panel",
            "xfdesktop",
        ],
    },
    Plan {
        manager: "apk",
        args: &["add"],
        packages: &[
            "tigervnc",
            "xfce4-session",
            "xfwm4",
            "xfce4-panel",
            "xfdesktop",
            "dbus-x11",
        ],
    },
];

/// `name` on `path_var`.
#[must_use]
pub fn find(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The plan for this host: the first package manager found.
#[must_use]
pub fn plan(path_var: Option<&OsStr>) -> Option<(Plan, PathBuf)> {
    PLANS
        .iter()
        .find_map(|plan| find(plan.manager, path_var).map(|path| (plan.clone(), path)))
}

/// How the install runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// Directly (root).
    AsRoot,
    /// Through `sudo` at this path.
    Sudo(PathBuf),
    /// Not at all: print the command (neither root nor `sudo`).
    PrintOnly,
}

/// How a user `is_root` with `sudo` (or not) runs the install.
#[must_use]
pub fn how(is_root: bool, sudo: Option<PathBuf>) -> Run {
    match (is_root, sudo) {
        (true, _) => Run::AsRoot,
        (false, Some(sudo)) => Run::Sudo(sudo),
        (false, None) => Run::PrintOnly,
    }
}

/// The command line of `plan` with `manager` (prefixed by `sudo`).
#[must_use]
pub fn command_line(plan: &Plan, manager: &Path, sudo: bool) -> Vec<String> {
    let mut line = Vec::new();
    if sudo {
        line.push("sudo".to_owned());
    }
    line.push(manager.display().to_string());
    line.extend(plan.args.iter().map(|a| (*a).to_owned()));
    line.extend(plan.packages.iter().map(|p| (*p).to_owned()));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_root_or_sudo_the_command_is_only_printed() {
        assert_eq!(how(false, None), Run::PrintOnly);
        assert_eq!(how(true, None), Run::AsRoot);
        let sudo = PathBuf::from("/usr/bin/sudo");
        assert_eq!(how(false, Some(sudo.clone())), Run::Sudo(sudo));
        let line = command_line(&PLANS[0], Path::new("/usr/bin/apt-get"), true);
        assert_eq!(line[0], "sudo");
        assert!(line.contains(&"tigervnc-standalone-server".to_owned()));
    }
}
