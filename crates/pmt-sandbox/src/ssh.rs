//! The `ssh` backend's client side (spec: execution-environments, Backend
//! readiness checks; Remote file sync and sync-back).
//!
//! Every remote call is one `ssh` client process in batch mode (never a
//! password prompt) over a multiplexed master connection, so after the
//! first call a command costs one channel, not one handshake. The client
//! runs with the [`crate::container::CLIENT_ENV`] allowlist; the remote
//! command is a single POSIX-quoted string, as `ssh` hands it to the remote
//! login shell.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pmt_config::schema::SshConfig;
use pmt_core::error::{Error, codes};

/// The longest control socket path the client accepts (`sun_path` is 104
/// bytes on macOS; `%C` expands to 40 hex digits).
const MAX_CONTROL_DIR: usize = 60;

/// One configured SSH destination.
#[derive(Debug, Clone)]
pub struct SshTarget {
    program: PathBuf,
    destination: String,
    port: u32,
    config_file: Option<PathBuf>,
    control_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

impl SshTarget {
    /// The destination of `[terminal.ssh]`; `control_dir` (private, short)
    /// holds the multiplexing socket.
    ///
    /// # Errors
    ///
    /// `terminal.backend_unavailable` when host or user is unset, or no
    /// `ssh` client is installed.
    pub fn from_config(config: &SshConfig, control_dir: Option<&Path>) -> Result<Self, Error> {
        let (Some(host), Some(user)) = (
            config.host.as_deref().filter(|h| !h.is_empty()),
            config.user.as_deref().filter(|u| !u.is_empty()),
        ) else {
            return Err(Error::new(
                &codes::TERMINAL_BACKEND_UNAVAILABLE,
                "ssh backend requires terminal.ssh.host and terminal.ssh.user",
            ));
        };
        let env = crate::container::client_env();
        let path = env
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let exe = if cfg!(windows) { "ssh.exe" } else { "ssh" };
        let program = std::env::split_paths(&path)
            .map(|d| d.join(exe))
            .find(|p| p.is_file())
            .ok_or_else(|| {
                Error::new(
                    &codes::TERMINAL_BACKEND_UNAVAILABLE,
                    "ssh backend requires an ssh client on PATH",
                )
            })?;
        let control_dir = control_dir
            .filter(|d| cfg!(unix) && d.as_os_str().len() <= MAX_CONTROL_DIR)
            .map(Path::to_path_buf);
        Ok(Self {
            program,
            destination: format!("{user}@{host}"),
            port: config.port,
            config_file: config.config_file.as_ref().map(PathBuf::from),
            control_dir,
            env,
        })
    }

    /// `user@host`.
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// The client command running `argv` on the remote host (each element
    /// quoted for the remote shell).
    #[must_use]
    pub fn command<S: AsRef<str>>(&self, argv: &[S]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(n, v)| (n, v)))
            .stdin(Stdio::null());
        if let Some(file) = &self.config_file {
            command.arg("-F").arg(file);
        }
        command.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15"]);
        command.args(["-o", "ServerAliveInterval=15", "-p", &self.port.to_string()]);
        if let Some(dir) = &self.control_dir {
            command
                .arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg(format!("ControlPath={}/%C", dir.display()))
                .arg("-o")
                .arg("ControlPersist=60");
        }
        command.arg(&self.destination).arg("--");
        command.arg(quote_all(argv));
        command
    }
}

/// `argv` as one POSIX shell command line.
#[must_use]
pub fn quote_all<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` single-quoted for a POSIX shell.
#[must_use]
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_remote_shell() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(
            quote_all(&["sh", "-c", "echo $HOME"]),
            "'sh' '-c' 'echo $HOME'"
        );
    }

    #[test]
    fn an_unset_user_is_a_readiness_failure() {
        let config = SshConfig {
            host: Some("example.org".into()),
            ..SshConfig::default()
        };
        let err = SshTarget::from_config(&config, None).unwrap_err();
        assert_eq!(
            err.message(),
            "ssh backend requires terminal.ssh.host and terminal.ssh.user"
        );
    }
}
