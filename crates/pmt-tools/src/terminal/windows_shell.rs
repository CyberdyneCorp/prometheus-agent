//! Shell resolution for the Windows `local` backend (spec:
//! execution-environments, Windows local shell).
//!
//! `auto` takes Git Bash from `terminal.windows_bash_path`, then the Git
//! for Windows install locations, then `bash.exe` on `PATH` excluding the
//! WSL launcher (`System32\bash.exe`) and `WindowsApps` aliases (both start
//! a Linux distribution, not a Windows shell); otherwise `pwsh`, then
//! Windows PowerShell, with a one-time notice that POSIX syntax is
//! unavailable. An explicitly selected shell that is absent makes the
//! terminal tool unavailable. The logic is plain path handling so it is
//! testable on every OS; [`Probe::from_launch`] fills in the real places.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use pmt_config::schema::WindowsShell;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

use super::shell::{Shell, ShellKind, find_all};

/// Where to look.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// `terminal.windows_bash_path`.
    pub bash_path: Option<PathBuf>,
    /// Git for Windows `bash.exe` install locations, in order.
    pub git_bash: Vec<PathBuf>,
    /// The child `PATH`.
    pub path_var: Option<OsString>,
    /// `PATHEXT`.
    pub pathext: Option<OsString>,
    /// PowerShell 7 install locations tried after `PATH`.
    pub pwsh: Vec<PathBuf>,
    /// Windows PowerShell locations tried before `PATH`.
    pub powershell: Vec<PathBuf>,
}

impl Probe {
    /// The real locations from the launch environment.
    #[must_use]
    pub fn from_launch(launch: &LaunchEnv, bash_path: Option<&str>) -> Self {
        let var = |name: &str| launch.inherited(name).map(PathBuf::from);
        let program_files: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|name| var(name))
            .collect();
        let mut git_bash: Vec<PathBuf> = program_files
            .iter()
            .map(|p| p.join(r"Git\bin\bash.exe"))
            .collect();
        if let Some(local) = launch.local_app_data() {
            git_bash.push(local.join(r"Programs\Git\bin\bash.exe"));
        }
        let system_root = var("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        Self {
            bash_path: bash_path.map(PathBuf::from),
            git_bash,
            path_var: launch.inherited("PATH").map(OsStr::to_owned),
            pathext: launch.inherited("PATHEXT").map(OsStr::to_owned),
            pwsh: program_files
                .iter()
                .map(|p| p.join(r"PowerShell\7\pwsh.exe"))
                .collect(),
            powershell: vec![system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe")],
        }
    }

    fn on_path(&self, name: &str) -> Vec<PathBuf> {
        self.path_var
            .as_deref()
            .map(|path| {
                find_all(
                    name,
                    path,
                    Some(self.pathext.as_deref().unwrap_or(OsStr::new(".EXE"))),
                )
            })
            .unwrap_or_default()
    }

    fn git_bash(&self) -> Option<PathBuf> {
        let configured = self
            .bash_path
            .iter()
            .chain(&self.git_bash)
            .find(|p| p.is_file())
            .cloned();
        configured.or_else(|| {
            self.on_path("bash")
                .into_iter()
                .find(|p| !is_wsl_launcher(p))
        })
    }

    fn pwsh(&self) -> Option<PathBuf> {
        let on_path = self.on_path("pwsh").into_iter();
        on_path
            .chain(self.pwsh.iter().cloned())
            .find(|p| p.is_file() && !is_app_alias(p))
    }

    fn powershell(&self) -> Option<PathBuf> {
        let on_path = self.on_path("powershell").into_iter();
        self.powershell
            .iter()
            .cloned()
            .chain(on_path)
            .find(|p| p.is_file())
    }
}

/// `System32\bash.exe` (and `Sysnative`), or a `WindowsApps` alias: these
/// start WSL, not a Windows shell.
#[must_use]
pub fn is_wsl_launcher(path: &Path) -> bool {
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .map(OsStr::to_string_lossy);
    let system = parent
        .is_some_and(|p| p.eq_ignore_ascii_case("System32") || p.eq_ignore_ascii_case("Sysnative"));
    system || is_app_alias(path)
}

fn is_app_alias(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
    })
}

/// The shell `choice` selects.
///
/// # Errors
///
/// `terminal.shell_not_found` (exit 69) `<shell> not found` with the
/// install hint, for an explicitly selected shell that is absent or when
/// `auto` finds no shell at all.
pub fn resolve(choice: WindowsShell, probe: &Probe) -> Result<Shell, Error> {
    let shell = |kind, path| Shell {
        kind,
        path,
        notice: None,
    };
    match choice {
        WindowsShell::GitBash => probe
            .git_bash()
            .map(|p| shell(ShellKind::Bash, p))
            .ok_or_else(|| not_found("git-bash")),
        WindowsShell::Pwsh => probe
            .pwsh()
            .map(|p| shell(ShellKind::Pwsh, p))
            .ok_or_else(|| not_found("pwsh")),
        WindowsShell::Powershell => probe
            .powershell()
            .map(|p| shell(ShellKind::PowerShell, p))
            .ok_or_else(|| not_found("powershell")),
        WindowsShell::Auto => {
            if let Some(path) = probe.git_bash() {
                return Ok(shell(ShellKind::Bash, path));
            }
            let fallback = probe
                .pwsh()
                .map(|p| (ShellKind::Pwsh, p))
                .or_else(|| probe.powershell().map(|p| (ShellKind::PowerShell, p)))
                .ok_or_else(|| not_found("git-bash"))?;
            Ok(Shell {
                kind: fallback.0,
                path: fallback.1,
                notice: Some(format!(
                    "Git Bash was not found, so commands run in {}: POSIX shell syntax is unavailable. {}",
                    fallback.0.name(),
                    GIT_HINT
                )),
            })
        }
    }
}

const GIT_HINT: &str =
    "Install Git for Windows (https://git-scm.com/download/win) or set terminal.windows_bash_path.";

fn not_found(shell: &str) -> Error {
    let hint = match shell {
        "git-bash" => GIT_HINT,
        "pwsh" => "Install PowerShell 7: winget install --id Microsoft.PowerShell",
        _ => {
            "Windows PowerShell ships with Windows: check %SystemRoot%\\System32\\WindowsPowerShell\\v1.0"
        }
    };
    Error::new(
        &codes::TERMINAL_SHELL_NOT_FOUND,
        format!("{shell} not found"),
    )
    .with_hint(hint)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use pmt_config::schema::WindowsShell;

    use super::{Probe, is_wsl_launcher, resolve};
    use crate::terminal::shell::ShellKind;

    fn file(path: &Path) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
        path.to_owned()
    }

    fn temp() -> tempfile::TempDir {
        let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp");
        fs::create_dir_all(base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    #[test]
    fn wsl_launchers_are_recognised() {
        assert!(is_wsl_launcher(Path::new("C:/Windows/System32/bash.exe")));
        assert!(is_wsl_launcher(Path::new(
            "C:/Users/me/AppData/Local/Microsoft/WindowsApps/bash.exe"
        )));
        assert!(!is_wsl_launcher(Path::new(
            "C:/Program Files/Git/bin/bash.exe"
        )));
    }

    #[test]
    fn only_wsl_bash_on_path_falls_back_to_powershell() {
        let temp = temp();
        let system32 = temp.path().join("Windows/System32");
        file(&system32.join("bash.exe"));
        let apps = temp.path().join("WindowsApps");
        file(&apps.join("bash.exe"));
        let powershell = file(&system32.join("WindowsPowerShell/v1.0/powershell.exe"));
        let probe = Probe {
            path_var: Some(std::env::join_paths([&system32, &apps]).unwrap()),
            pathext: Some(".EXE".into()),
            git_bash: vec![temp.path().join("Git/bin/bash.exe")],
            powershell: vec![powershell.clone()],
            ..Probe::default()
        };
        let shell = resolve(WindowsShell::Auto, &probe).unwrap();
        assert_eq!(shell.kind, ShellKind::PowerShell);
        assert_eq!(shell.path, powershell);
        assert!(
            shell
                .notice
                .unwrap()
                .contains("POSIX shell syntax is unavailable")
        );
        let err = resolve(WindowsShell::GitBash, &probe).unwrap_err();
        assert_eq!((err.message(), err.exit_code()), ("git-bash not found", 69));
    }

    #[test]
    fn git_bash_wins_and_pwsh_beats_windows_powershell() {
        let temp = temp();
        let git = file(&temp.path().join("Git/bin/bash.exe"));
        let pwsh = file(&temp.path().join("PowerShell/7/pwsh.exe"));
        let probe = Probe {
            git_bash: vec![git.clone()],
            pwsh: vec![pwsh.clone()],
            powershell: vec![file(&temp.path().join("ps/powershell.exe"))],
            ..Probe::default()
        };
        let auto = resolve(WindowsShell::Auto, &probe).unwrap();
        assert_eq!(
            (auto.kind, auto.path, auto.notice),
            (ShellKind::Bash, git, None)
        );
        let explicit = resolve(WindowsShell::Pwsh, &probe).unwrap();
        assert_eq!(explicit.path, pwsh);
        let missing = Probe::default();
        assert_eq!(
            resolve(WindowsShell::Pwsh, &missing).unwrap_err().message(),
            "pwsh not found"
        );
    }
}
