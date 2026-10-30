//! The local shell and its `prometheus doctor terminal` checks (spec:
//! execution-environments, Backend readiness checks; Windows local shell).

use pmt_config::schema::TerminalConfig;
use pmt_core::doctor::{Check, Status};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_sandbox::Sandbox;
use pmt_sandbox::backend::{BackendRegistry, Readiness};

use super::shell::{self, Shell, ShellKind};
use super::windows_shell::{self, Probe};

/// The shell the `local` backend uses on this host.
///
/// # Errors
///
/// `terminal.shell_not_found` (Windows) for an absent selected shell.
pub fn local_shell(config: &TerminalConfig, launch: &LaunchEnv) -> Result<Shell, Error> {
    if cfg!(windows) {
        let probe = Probe::from_launch(launch, config.windows_bash_path.as_deref());
        windows_shell::resolve(config.windows_shell, &probe)
    } else {
        Ok(shell::resolve_unix(launch.inherited("PATH")))
    }
}

/// POSIX-only features unavailable with `shell` on this OS. `pty` is not
/// implemented yet on any OS (background processes use pipes).
#[must_use]
pub fn degraded_features(shell: &Shell, windows: bool) -> Vec<&'static str> {
    let mut degraded = Vec::new();
    if !shell.kind.is_posix() {
        degraded.push("persistent shell");
    }
    degraded.push("pty");
    if windows {
        degraded.push("signals");
    }
    degraded
}

/// The `terminal/shell` doctor check: the chosen shell and what is
/// degraded, or why the terminal tool is unavailable.
#[must_use]
pub fn shell_check(shell: &Result<Shell, Error>, windows: bool) -> Check {
    match shell {
        Err(err) => {
            let check = Check::new("terminal", "shell", Status::Fail, err.message());
            match err.hint() {
                Some(hint) => check.with_hint(hint),
                None => check,
            }
        }
        Ok(shell) => {
            let chosen = format!("shell: {} ({})", shell.kind.name(), shell.path.display());
            let degraded = degraded_features(shell, windows);
            let check = Check::new(
                "terminal",
                "shell",
                Status::Warn,
                format!("{chosen}; degraded: {}", degraded.join(", ")),
            );
            if shell.kind == ShellKind::Bash {
                check
            } else {
                check.with_hint(
                    "install Git for Windows for a POSIX shell, or set terminal.windows_bash_path",
                )
            }
        }
    }
}

/// Readiness of the configured backend plus, for `local`, the shell: the
/// `terminal` tool's availability probe and `prometheus doctor terminal`.
#[must_use]
pub fn readiness(
    registry: &BackendRegistry,
    config: &TerminalConfig,
    sandbox: &Sandbox,
    shell: &Result<Shell, Error>,
) -> Readiness {
    let mut readiness = registry.readiness(config, sandbox);
    if config.backend == "local" {
        readiness.checks.push(shell_check(shell, cfg!(windows)));
    }
    readiness
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pmt_core::doctor::Status;

    use super::shell_check;
    use crate::terminal::shell::{Shell, ShellKind};

    #[test]
    fn powershell_on_windows_reports_every_degraded_posix_feature() {
        let shell = Shell {
            kind: ShellKind::Pwsh,
            path: PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            notice: None,
        };
        let check = shell_check(&Ok(shell), true);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.starts_with("shell: pwsh ("),
            "{}",
            check.message
        );
        assert!(
            check
                .message
                .ends_with("degraded: persistent shell, pty, signals")
        );
        let bash = Shell {
            kind: ShellKind::Bash,
            path: PathBuf::from("/bin/bash"),
            notice: None,
        };
        let check = shell_check(&Ok(bash), false);
        assert_eq!(check.message, "shell: bash (/bin/bash); degraded: pty");
    }

    #[test]
    fn an_absent_shell_fails_the_check() {
        let err = crate::terminal::windows_shell::resolve(
            pmt_config::schema::WindowsShell::Pwsh,
            &crate::terminal::windows_shell::Probe::default(),
        )
        .unwrap_err();
        let check = shell_check(&Err(err), true);
        assert_eq!(
            (check.status, check.message.as_str()),
            (Status::Fail, "pwsh not found")
        );
        assert!(check.hint.unwrap().contains("winget"));
    }
}
