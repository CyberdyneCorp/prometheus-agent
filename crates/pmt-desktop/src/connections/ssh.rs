//! `ssh` connections (spec: desktop-app, SSH connections reuse the local
//! ticket model): the system `ssh` client with strict host-key checking
//! against the app's own `known_hosts` runs `prometheus daemon ensure` and
//! `prometheus daemon ticket` on the remote host, and `ssh -W` carries
//! each dial to the daemon's loopback endpoint there, so the remote host
//! needs no network-exposed listener. An unknown host key is shown for
//! confirmation; a changed one is an error the shell latches.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, Join, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::SshTarget;
use super::tls::BoxStream;
use crate::ensure::EnsureReport;

/// How long one remote command may take (`daemon ensure` starts a daemon).
const COMMAND_BOUND: Duration = Duration::from_secs(90);
/// What `ssh` exits with when it, not the remote command, failed.
const SSH_FAILED: i32 = 255;
/// The remote shell's "command not found".
const NOT_FOUND: i32 = 127;

/// The message of a changed host key.
pub const HOST_KEY_CHANGED: &str = "The host key has changed since you last connected";
/// The message of a missing remote binary.
pub const NOT_INSTALLED: &str = "Prometheus is not installed on the remote host";

/// Which programs run.
#[derive(Debug, Clone)]
pub struct SshPrograms {
    /// The `ssh` client (`desktop.ssh_path` on Windows).
    pub ssh: PathBuf,
    /// `ssh-keyscan`, which reads a host's key for confirmation.
    pub keyscan: PathBuf,
}

impl Default for SshPrograms {
    fn default() -> Self {
        Self {
            ssh: PathBuf::from("ssh"),
            keyscan: PathBuf::from("ssh-keyscan"),
        }
    }
}

impl SshPrograms {
    /// The programs for `ssh` (a configured path: `ssh-keyscan` beside it).
    #[must_use]
    pub fn at(ssh: &Path) -> Self {
        let keyscan = ssh.with_file_name(if cfg!(windows) {
            "ssh-keyscan.exe"
        } else {
            "ssh-keyscan"
        });
        Self {
            ssh: ssh.to_path_buf(),
            keyscan: if ssh.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
                keyscan
            } else {
                PathBuf::from("ssh-keyscan")
            },
        }
    }
}

/// Why an SSH step failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshFailure {
    /// First use: the user confirms this host key.
    HostKeyUnknown {
        /// The presented key's `SHA256:` fingerprint.
        fingerprint: String,
    },
    /// The host presented another key than the trusted one.
    HostKeyChanged {
        /// The new key's fingerprint.
        fingerprint: String,
    },
    /// `prometheus` is not on the remote non-interactive `PATH`.
    NotInstalled,
    /// Anything else, with what `ssh` or the remote command said.
    Failed(String),
}

impl std::fmt::Display for SshFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostKeyUnknown { fingerprint } => {
                write!(f, "Confirm the host key before connecting: {fingerprint}")
            }
            Self::HostKeyChanged { fingerprint } => {
                write!(f, "{HOST_KEY_CHANGED} (new key {fingerprint})")
            }
            Self::NotInstalled => write!(
                f,
                "{NOT_INSTALLED}: set the remote binary path if it is installed outside the non-interactive PATH"
            ),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

/// What `daemon ticket --json` prints.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteTicket {
    /// The one-use ticket.
    pub ticket: String,
    /// The endpoint it opens (the daemon may have restarted on a new port).
    pub rpc_url: String,
}

/// The SSH side of the shell.
#[derive(Debug, Clone)]
pub struct Ssh {
    programs: SshPrograms,
    dir: PathBuf,
}

impl Ssh {
    /// SSH with the app's `known_hosts` (and control sockets) in `dir`.
    #[must_use]
    pub fn new(programs: SshPrograms, dir: &Path) -> Self {
        Self {
            programs,
            dir: dir.to_path_buf(),
        }
    }

    fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    /// The options every `ssh` run shares: strict host-key checking
    /// against the app's own file, no prompts, one master connection
    /// reused by the commands and the dials of a target (Unix only).
    fn base(&self, target: &SshTarget) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut option = |o: String| {
            args.push("-o".into());
            args.push(o.into());
        };
        option("BatchMode=yes".into());
        option("StrictHostKeyChecking=yes".into());
        option(format!(
            "UserKnownHostsFile={}",
            self.known_hosts().display()
        ));
        option("ConnectTimeout=10".into());
        if cfg!(unix) {
            option("ControlMaster=auto".into());
            option(format!("ControlPath={}", self.dir.join("cm-%C").display()));
            option("ControlPersist=60".into());
        }
        args.push("-p".into());
        args.push(target.port.to_string().into());
        if let Some(user) = &target.user {
            args.push("-l".into());
            args.push(user.into());
        }
        args.push(target.host.clone().into());
        args
    }

    fn remote_command(target: &SshTarget, profile: Option<&str>, words: &[&str]) -> String {
        let binary = target.binary.as_deref().unwrap_or("prometheus");
        let mut line = quote(binary);
        line.push_str(" --json");
        if let Some(profile) = profile.or(target.profile.as_deref()) {
            line.push_str(" --profile ");
            line.push_str(&quote(profile));
        }
        for word in words {
            line.push(' ');
            line.push_str(word);
        }
        line
    }

    async fn run(
        &self,
        target: &SshTarget,
        profile: Option<&str>,
        words: &[&str],
    ) -> Result<String, SshFailure> {
        let mut args = self.base(target);
        args.push("--".into());
        args.push(Self::remote_command(target, profile, words).into());
        let child = Command::new(&self.programs.ssh)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SshFailure::Failed(format!("cannot run ssh: {e}")))?;
        let out = tokio::time::timeout(COMMAND_BOUND, child.wait_with_output())
            .await
            .map_err(|_| SshFailure::Failed("the ssh command timed out".to_owned()))?
            .map_err(|e| SshFailure::Failed(e.to_string()))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            Some(SSH_FAILED) => Err(self.ssh_failed(target, &stderr).await),
            Some(NOT_FOUND) => Err(SshFailure::NotInstalled),
            _ if stderr.contains("command not found") || stderr.contains("not recognized") => {
                Err(SshFailure::NotInstalled)
            }
            code => Err(SshFailure::Failed(format!(
                "the remote command failed (exit {}): {}",
                code.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                last_line(&stderr)
            ))),
        }
    }

    async fn ssh_failed(&self, target: &SshTarget, stderr: &str) -> SshFailure {
        let changed = stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED");
        let unknown =
            stderr.contains("Host key verification failed") || stderr.contains("host key is known");
        if !changed && !unknown {
            return SshFailure::Failed(format!("ssh: {}", last_line(stderr)));
        }
        let fingerprint = match self.scan(target).await {
            Ok(keys) => keys
                .first()
                .map_or_else(|| "unknown".to_owned(), |k| k.fingerprint.clone()),
            Err(e) => return SshFailure::Failed(e),
        };
        if changed {
            SshFailure::HostKeyChanged { fingerprint }
        } else {
            SshFailure::HostKeyUnknown { fingerprint }
        }
    }

    /// Runs `daemon ensure` on the remote host: its daemon's endpoint.
    ///
    /// # Errors
    ///
    /// [`SshFailure`].
    pub async fn ensure(&self, target: &SshTarget) -> Result<EnsureReport, SshFailure> {
        let out = self.run(target, None, &["daemon", "ensure"]).await?;
        serde_json::from_str(out.trim()).map_err(|_| {
            SshFailure::Failed(format!(
                "the remote daemon ensure printed no endpoint: {}",
                last_line(&out)
            ))
        })
    }

    /// A one-use dial ticket of the remote daemon for `profile` (the
    /// connection's own profile when `None`; `daemon ticket`).
    ///
    /// # Errors
    ///
    /// [`SshFailure`].
    pub async fn ticket(
        &self,
        target: &SshTarget,
        profile: Option<&str>,
    ) -> Result<RemoteTicket, SshFailure> {
        let out = self.run(target, profile, &["daemon", "ticket"]).await?;
        serde_json::from_str(out.trim())
            .map_err(|_| SshFailure::Failed("the remote daemon gave no ticket".to_owned()))
    }

    /// The remote daemon's state (`daemon status`): never starts it.
    ///
    /// # Errors
    ///
    /// [`SshFailure`].
    pub async fn status(&self, target: &SshTarget) -> Result<serde_json::Value, SshFailure> {
        let out = self.run(target, None, &["daemon", "status"]).await?;
        serde_json::from_str(out.trim())
            .map_err(|_| SshFailure::Failed("the remote daemon status did not parse".to_owned()))
    }

    /// A stream to `port` on the remote host's loopback (`ssh -W`).
    ///
    /// # Errors
    ///
    /// `ssh` could not start.
    pub fn pipe(&self, target: &SshTarget, port: u16) -> Result<BoxStream, String> {
        let mut args = self.base(target);
        args.push("-W".into());
        args.push(format!("127.0.0.1:{port}").into());
        let mut child = Command::new(&self.programs.ssh)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            return Err("ssh gave no pipes".to_owned());
        };
        Ok(Box::new(Piped {
            _child: child,
            io: tokio::io::join(stdout, stdin),
        }))
    }

    /// The host's keys as `ssh-keyscan` reads them, Ed25519 first.
    async fn scan(&self, target: &SshTarget) -> Result<Vec<HostKey>, String> {
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            Command::new(&self.programs.keyscan)
                .args(["-T", "10", "-p", &target.port.to_string(), &target.host])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "reading the host key timed out".to_owned())?
        .map_err(|e| format!("cannot run ssh-keyscan: {e}"))?;
        let mut keys: Vec<HostKey> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(HostKey::parse)
            .collect();
        keys.sort_by_key(|k| k.kind != "ssh-ed25519");
        if keys.is_empty() {
            return Err(format!("{} presented no host key", target.host));
        }
        Ok(keys)
    }

    /// Trusts the host key the user confirmed: scanned again, it must
    /// still be the one with `fingerprint`; earlier keys of the host are
    /// replaced.
    ///
    /// # Errors
    ///
    /// The host now presents another key, or the file could not be
    /// written.
    pub async fn trust(&self, target: &SshTarget, fingerprint: &str) -> Result<(), String> {
        let keys = self.scan(target).await?;
        let key = keys
            .iter()
            .find(|k| k.fingerprint == fingerprint)
            .ok_or("the host now presents another key than the one you confirmed")?;
        let pattern = if target.port == 22 {
            target.host.clone()
        } else {
            format!("[{}]:{}", target.host, target.port)
        };
        let file = self.known_hosts();
        let kept: Vec<String> = std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split_whitespace().next() != Some(pattern.as_str()))
            .map(str::to_owned)
            .collect();
        let mut text = kept.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = writeln!(text, "{pattern} {} {}", key.kind, key.blob);
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        pmt_core::private_fs::write_atomic(&file, text.as_bytes()).map_err(|e| e.to_string())
    }
}

/// One `known_hosts`-style line of `ssh-keyscan`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostKey {
    kind: String,
    blob: String,
    fingerprint: String,
}

impl HostKey {
    fn parse(line: &str) -> Option<Self> {
        if line.starts_with('#') {
            return None;
        }
        let mut words = line.split_whitespace();
        let (_host, kind, blob) = (words.next()?, words.next()?, words.next()?);
        Some(Self {
            kind: kind.to_owned(),
            blob: blob.to_owned(),
            fingerprint: host_key_fingerprint(blob)?,
        })
    }
}

/// `SHA256:<base64>` of a key blob, as `ssh-keygen -l` prints it.
#[must_use]
pub fn host_key_fingerprint(blob: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .ok()?;
    let digest = Sha256::digest(&bytes);
    Some(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
    ))
}

/// A word for the remote POSIX shell.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(nothing printed)")
        .to_owned()
}

/// `ssh -W`'s pipes as one stream; the child lives as long as they do.
struct Piped {
    _child: Child,
    io: Join<ChildStdout, ChildStdin>,
}

impl AsyncRead for Piped {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Piped {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{HostKey, quote};

    #[test]
    fn remote_words_are_quoted_for_the_shell() {
        assert_eq!(quote("prometheus"), "'prometheus'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_scanned_key_has_the_ssh_keygen_fingerprint() {
        // `ssh-keygen -lf` of this key prints the same SHA256 value.
        let key = HostKey::parse(
            "host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
        )
        .unwrap();
        assert_eq!(key.kind, "ssh-ed25519");
        assert_eq!(
            key.fingerprint,
            "SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU"
        );
        assert!(HostKey::parse("# host comment").is_none());
    }
}
