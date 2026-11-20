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

/// How long a reverse forward may take to be granted.
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A port on the remote host's loopback forwarded back to a local
/// address, on an SSH connection of its own (never the multiplexed
/// master: every client asking it again for the same forward would fail).
/// Dropping it closes the connection and the remote port.
#[derive(Debug)]
pub struct ReverseForward {
    child: std::process::Child,
    port: u16,
}

impl ReverseForward {
    /// The remote loopback port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ReverseForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl SshTarget {
    /// Forwards a port the server allocates on its loopback to `local`.
    ///
    /// # Errors
    ///
    /// `terminal.backend_unavailable` naming why: the client did not
    /// start, the server refused the forward (`AllowTcpForwarding`), or no
    /// port was granted within 20 s.
    pub fn reverse_forward(&self, local: std::net::SocketAddr) -> Result<ReverseForward, Error> {
        let mut command = self.forward_client();
        command.stderr(Stdio::piped());
        command
            .arg("-R")
            .arg(format!("127.0.0.1:0:{local}"))
            .arg(&self.destination);
        let refused = |why: String| {
            Error::new(
                &codes::TERMINAL_BACKEND_UNAVAILABLE,
                format!(
                    "cannot forward a port of {} back to {local}: {why}",
                    self.destination
                ),
            )
        };
        let mut child = command.spawn().map_err(|e| refused(e.to_string()))?;
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            return Err(refused("no client output".to_owned()));
        };
        let (lines, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            // Read to the end: the client must never block on a full pipe.
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                let _ = lines.send(line);
            }
        });
        let deadline = std::time::Instant::now() + FORWARD_TIMEOUT;
        let mut said = Vec::new();
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match received.recv_timeout(left) {
                Ok(line) => {
                    if let Some(port) = allocated_port(&line) {
                        return Ok(ReverseForward { child, port });
                    }
                    said.push(line);
                }
                // The client exited: its stderr is closed.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let why = said.join("; ");
        Err(refused(if why.is_empty() {
            "no port was granted".to_owned()
        } else {
            why
        }))
    }

    /// A client that only forwards (`-N`), on a connection of its own (see
    /// [`ReverseForward`]), failing when a forward cannot be set up.
    fn forward_client(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(n, v)| (n, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if let Some(file) = &self.config_file {
            command.arg("-F").arg(file);
        }
        // INFO: the client reports the allocated port at that level.
        for option in [
            "BatchMode=yes",
            "ConnectTimeout=15",
            "ServerAliveInterval=15",
            "ExitOnForwardFailure=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "LogLevel=INFO",
        ] {
            command.arg("-o").arg(option);
        }
        command.args(["-N", "-T", "-p", &self.port.to_string()]);
        command
    }

    /// The client relaying each local Unix socket of `forwards` to its
    /// remote Unix socket (`-L <local>:<remote>`), replacing a stale local
    /// socket; it runs until killed.
    #[must_use]
    pub fn socket_forwards(&self, forwards: &[(PathBuf, String)]) -> Command {
        let mut command = self.forward_client();
        command.args([
            "-o",
            "StreamLocalBindUnlink=yes",
            "-o",
            "StreamLocalBindMask=0177",
        ]);
        for (local, remote) in forwards {
            command
                .arg("-L")
                .arg(format!("{}:{remote}", local.display()));
        }
        command.arg(&self.destination);
        command
    }
}

/// The port of the client's "Allocated port N for remote forward" line.
fn allocated_port(line: &str) -> Option<u16> {
    line.trim()
        .strip_prefix("Allocated port ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
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
