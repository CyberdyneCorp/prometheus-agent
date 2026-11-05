//! The `container` and `ssh` backends' side of the terminal (spec:
//! execution-environments, Container lifecycle and orphan reaping;
//! Infrastructure failures are degraded results; Backend isolation
//! descriptor).
//!
//! A [`Transport`] turns a remote argv into a local client process (`podman
//! exec`, `ssh`). [`RemoteTarget`] runs the terminal's shell scripts through
//! it inside a small launcher that puts the command in its own remote
//! process group and records the group leader's pid, so a timeout, `kill`
//! or session end signals the remote tree too, not only the local client.
//! The child environment keeps only what makes sense remotely: host
//! locations (`PATH`, `HOME`, the egress proxy, the CA bundle) stay behind.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_sandbox::backend::Descriptor;
use pmt_sandbox::container::run_bounded;

use super::env::ChildEnv;
use super::shell::{Shell, ShellKind};
use super::spawn::{Group, RemoteGroup, SpawnError, Spawned};
use super::target::Target;

pub mod container;
pub mod exec;
pub mod ssh;

pub use exec::{ExecResult, RemoteExec};

/// Host-side variables a remote child never receives.
const HOST_ONLY: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
];

/// How long a management call (probe, state file, kill) may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Records its own pid, runs the command and exits with its status. The
/// launcher leads the client's remote process group (`exec` and `sshd`
/// start it in a fresh one), so signalling that group stops the whole
/// command tree. Arguments: pid file, argv.
const LAUNCHER: &str = "echo \"$$\" > \"$1\"; __pmt_pidf=$1; shift\n\
    \"$@\"; __pmt_rc=$?\n\
    rm -f \"$__pmt_pidf\"\n\
    exit $__pmt_rc\n";

/// Signals the process group of the pid in a pid file (`/proc` on Linux,
/// `ps` elsewhere). Arguments: pid file, signal.
const KILLER: &str = "p=$(cat \"$1\" 2>/dev/null) && [ -n \"$p\" ] || exit 0\n\
    g=$(sed 's/.*) //' \"/proc/$p/stat\" 2>/dev/null | cut -d' ' -f3)\n\
    [ -n \"$g\" ] || g=$(ps -o pgid= -p \"$p\" 2>/dev/null | tr -d ' ')\n\
    [ -n \"$g\" ] && kill -s \"$2\" -- \"-$g\" 2>/dev/null; true";

/// One variable handed to a remote child.
#[derive(Debug, Clone)]
pub struct EnvVar {
    /// Its name.
    pub name: String,
    /// Its value.
    pub value: OsString,
    /// A secret: never on a command line.
    pub secret: bool,
}

/// A local client that runs argv on a remote backend.
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// `container <name>` / `ssh <user@host>`.
    fn describe(&self) -> String;
    /// Makes the environment reachable. Blocks.
    ///
    /// # Errors
    ///
    /// Why it is unreachable, or a hardening refusal.
    fn connect(&self) -> Result<(), SpawnError>;
    /// Forgets cached state after a failure.
    fn evict(&self);
    /// The client command running `argv` remotely with `env` (values may
    /// be secrets: never on the command line), stdin piped or closed.
    ///
    /// # Errors
    ///
    /// The environment cannot be handed over.
    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String>;
    /// The client's exit code for "could not reach the backend".
    fn failure_code(&self) -> i32;
    /// Session end. Blocks.
    fn close(&self);
}

/// What [`RemoteTarget::connect`] learnt about the remote side.
#[derive(Debug, Clone)]
struct Remote {
    shell: Shell,
    home: PathBuf,
    state: PathBuf,
}

/// Where a container keeps the shell state: its `/tmp` tmpfs.
pub const CONTAINER_STATE: &str = "/tmp/.prometheus-terminal";

/// Where an SSH host keeps the shell state.
pub const SSH_STATE: &str = "${HOME:-/tmp}/.cache/prometheus-terminal";

/// The container or SSH backend of one session.
#[derive(Debug)]
pub struct RemoteTarget {
    transport: Arc<dyn Transport>,
    descriptor: Descriptor,
    session: String,
    state_base: String,
    remote: Mutex<Option<Remote>>,
    runs: AtomicU64,
}

impl RemoteTarget {
    /// The backend over `transport`; the session's state files go to
    /// `<state_base>/<session>` (a shell expression: a container's root is
    /// read-only, so it uses its `/tmp` tmpfs; an SSH host its home).
    #[must_use]
    pub fn new(
        transport: Arc<dyn Transport>,
        descriptor: Descriptor,
        session: &str,
        state_base: &str,
    ) -> Self {
        Self {
            transport,
            descriptor,
            session: session.to_owned(),
            state_base: state_base.to_owned(),
            remote: Mutex::new(None),
            runs: AtomicU64::new(0),
        }
    }

    /// The transport (file tools and sync-back use it too).
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    fn remote(&self) -> Option<Remote> {
        self.remote
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs a short `sh -c` script and returns its standard output.
    fn probe(&self, script: &str, args: &[&str]) -> Result<String, String> {
        let result = exec::run(self.transport.as_ref(), script, args, None)?;
        if result.code == 0 {
            Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&result.stderr).trim().to_owned())
        }
    }

    fn discover(&self) -> Result<Remote, String> {
        let state = format!("{}/{}", self.state_base, self.session);
        let script = format!(
            "umask 077; mkdir -p \"{state}\" && cd \"{state}\" && pwd -P && echo \"${{HOME:-/}}\" && (command -v bash || command -v sh || echo /bin/sh)"
        );
        let text = self.probe(&script, &[])?;
        let mut lines = text.lines();
        let (Some(state), Some(home), Some(shell)) = (lines.next(), lines.next(), lines.next())
        else {
            return Err(format!(
                "unexpected reply from {}: {text}",
                self.transport.describe()
            ));
        };
        let kind = if shell.ends_with("bash") {
            ShellKind::Bash
        } else {
            ShellKind::Sh
        };
        Ok(Remote {
            shell: Shell {
                kind,
                path: PathBuf::from(shell),
                notice: None,
            },
            home: PathBuf::from(home),
            state: PathBuf::from(state),
        })
    }
}

/// `env`'s variables a remote child receives.
fn remote_env(env: &ChildEnv) -> Vec<EnvVar> {
    let secrets = env.secret_names();
    env.names()
        .filter_map(|name| {
            let secret = secrets.iter().any(|s| s == name);
            let host_only = HOST_ONLY.contains(&name) || is_session_plumbing(name);
            let value = env.get(name)?.to_owned();
            (secret || !host_only).then(|| EnvVar {
                name: name.to_owned(),
                value,
                secret,
            })
        })
        .collect()
}

/// Variables the session sets for its local children only: the egress
/// proxy and the CA bundle point at this host.
fn is_session_plumbing(name: &str) -> bool {
    pmt_net::egress::PROXY_VARS.contains(&name)
        || matches!(
            name,
            "SSL_CERT_FILE"
                | "REQUESTS_CA_BUNDLE"
                | "CURL_CA_BUNDLE"
                | "NODE_EXTRA_CA_CERTS"
                | "GIT_SSL_CAINFO"
                | "PIP_CERT"
        )
}

impl Target for RemoteTarget {
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        self.transport.connect()?;
        if self.remote().is_none() {
            let remote = self.discover().map_err(SpawnError::Infrastructure)?;
            *self.remote.lock().unwrap_or_else(PoisonError::into_inner) = Some(remote);
        }
        Ok(())
    }

    fn evict(&self) {
        self.transport.evict();
        *self.remote.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn shell(&self) -> Option<Shell> {
        self.remote().map(|r| r.shell)
    }

    fn home(&self) -> Option<PathBuf> {
        self.remote().map(|r| r.home)
    }

    fn state_dir(&self) -> PathBuf {
        self.remote()
            .map_or_else(|| PathBuf::from("/tmp"), |r| r.state)
    }

    fn spawn(
        &self,
        env: &ChildEnv,
        argv: &[OsString],
        _cwd: &Path,
        stdin: bool,
    ) -> Result<Spawned, SpawnError> {
        let run = self.runs.fetch_add(1, Ordering::Relaxed);
        let pidfile = self.state_dir().join(format!("pid-{run}"));
        let pidfile = pidfile.to_string_lossy().into_owned();
        let mut full = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            LAUNCHER.to_owned(),
            "pmt-launch".to_owned(),
            pidfile.clone(),
        ];
        full.extend(argv.iter().map(|a| a.to_string_lossy().into_owned()));
        let mut command = self
            .transport
            .command(&full, &remote_env(env), stdin)
            .map_err(SpawnError::Infrastructure)?;
        command
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let child = command.spawn().map_err(|err| {
            SpawnError::Infrastructure(format!(
                "cannot start the {} client: {err}",
                self.transport.describe()
            ))
        })?;
        let remote = Arc::new(PidGroup {
            transport: Arc::clone(&self.transport),
            pidfile,
        });
        let group = Group::local(child.id()).with_remote(remote);
        Ok(Spawned { child, group })
    }

    fn take_text(&self, path: &Path) -> Option<String> {
        let path = path.to_string_lossy();
        self.probe("cat -- \"$1\" && rm -f -- \"$1\"", &[&path])
            .ok()
    }

    fn is_file(&self, path: &Path) -> bool {
        let path = path.to_string_lossy();
        self.probe("test -f \"$1\"", &[&path]).is_ok()
    }

    fn remove(&self, path: &Path) {
        let path = path.to_string_lossy();
        let _ = self.probe("rm -f -- \"$1\"", &[&path]);
    }

    fn failure(&self, code: i32) -> Option<String> {
        if code != self.transport.failure_code() {
            return None;
        }
        // The client's failure code is also a valid command status: only a
        // backend that no longer answers makes it a backend failure.
        match self.probe("true", &[]) {
            Ok(_) => None,
            Err(why) => {
                self.evict();
                Some(format!(
                    "{} is unreachable: {why}",
                    self.transport.describe()
                ))
            }
        }
    }

    fn close(&self) {
        if let Some(remote) = self.remote() {
            let state = remote.state.to_string_lossy().into_owned();
            let _ = self.probe("rm -rf -- \"$1\"", &[&state]);
        }
        self.transport.close();
    }
}

/// The remote group behind one command.
#[derive(Debug)]
struct PidGroup {
    transport: Arc<dyn Transport>,
    pidfile: String,
}

impl RemoteGroup for PidGroup {
    fn signal(&self, kill: bool) {
        let signal = if kill { "KILL" } else { "TERM" };
        let argv = [
            "sh".to_owned(),
            "-c".to_owned(),
            KILLER.to_owned(),
            "pmt-kill".to_owned(),
            self.pidfile.clone(),
            signal.to_owned(),
        ];
        let Ok(command) = self.transport.command(&argv, &[], false) else {
            return;
        };
        // A drop or a timeout cannot wait for the round trip.
        std::thread::spawn(move || {
            let _ = run_bounded(command, CALL_TIMEOUT);
        });
    }
}
