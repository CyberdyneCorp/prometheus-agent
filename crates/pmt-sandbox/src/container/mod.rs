//! The `container` backend's runtime side (spec: execution-environments,
//! Container hardening is never silently dropped; Container lifecycle and
//! orphan reaping; Container backend options).
//!
//! - [`Runtime`] finds the Podman or Docker CLI and probes whether it is
//!   rootless.
//! - [`hardening`] is the flag set every container starts with and maps a
//!   runtime's rejection back to the flag it names.
//! - [`options`] validates `terminal.container.volumes` and `extra_args`.
//! - [`lifecycle`] creates, starts, stops and removes containers, keeps
//!   the process-wide pool idle reaper, and reaps a profile's orphans.
//!
//! Every runtime call runs the CLI with an environment built from an
//! allowlist ([`CLIENT_ENV`]), never a copy of this process's.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use pmt_config::schema::{DegradedFeature, SandboxConfig};
use pmt_core::error::{Error, codes};

pub mod hardening;
pub mod lifecycle;
pub mod options;

pub use lifecycle::{Activity, Container, ContainerSpec, Pool, reap_orphans};

/// Variables the runtime and SSH clients read for their own configuration
/// (sockets, contexts, config dirs); nothing else of this process reaches
/// them.
pub const CLIENT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "LANG",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "CONTAINER_HOST",
    "CONTAINER_CONNECTION",
    "CONTAINERS_CONF",
    "CONTAINERS_STORAGE_CONF",
    "SSH_AUTH_SOCK",
    "SYSTEMROOT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

/// The client environment: [`CLIENT_ENV`] taken from this process.
#[must_use]
pub fn client_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|n| CLIENT_ENV.iter().any(|a| a.eq_ignore_ascii_case(n)))
        })
        .collect()
}

/// How long one management call (`info`, `create`, `inspect`) may take.
const CALL_TIMEOUT: Duration = Duration::from_mins(2);

/// Which CLI a runtime is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Podman.
    Podman,
    /// Docker (or a Docker-compatible CLI).
    Docker,
}

/// A container runtime CLI.
#[derive(Debug, Clone)]
pub struct Runtime {
    program: PathBuf,
    flavor: Flavor,
    env: Vec<(OsString, OsString)>,
}

/// What [`Runtime::probe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The daemon cannot act as host root: rootless, or inside a VM.
    pub rootless: bool,
    /// How that was decided.
    pub how: String,
}

impl Probe {
    /// One line for doctor output.
    #[must_use]
    pub fn describe(&self) -> String {
        self.how.clone()
    }
}

impl Runtime {
    /// The runtime `terminal.container.runtime` names: `auto` (Podman,
    /// else Docker), `podman`, `docker`, or a path to either CLI.
    ///
    /// # Errors
    ///
    /// `terminal.container_runtime` when nothing is found.
    pub fn detect(setting: &str) -> Result<Self, Error> {
        let env = client_env();
        let path = env
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone());
        let lookup = |name: &str| path.as_deref().and_then(|p| find_on_path(name, p));
        let found = match setting {
            "auto" | "" => lookup("podman").or_else(|| lookup("docker")),
            "podman" | "docker" => lookup(setting),
            other => Some(PathBuf::from(other)).filter(|p| p.is_file()),
        };
        let program = found.ok_or_else(|| {
            Error::new(
                &codes::TERMINAL_CONTAINER_RUNTIME,
                format!("container runtime '{setting}' not found"),
            )
            .with_hint(
                "install rootless Podman or Docker, or set terminal.container.runtime to its path",
            )
        })?;
        Ok(Self::at(program, env))
    }

    /// The CLI at `program` with client environment `env`.
    #[must_use]
    pub fn at(program: PathBuf, env: Vec<(OsString, OsString)>) -> Self {
        let name = program
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let flavor = if name.contains("podman") {
            Flavor::Podman
        } else {
            Flavor::Docker
        };
        Self {
            program,
            flavor,
            env,
        }
    }

    /// `podman` or `docker`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self.flavor {
            Flavor::Podman => "podman",
            Flavor::Docker => "docker",
        }
    }

    /// Which CLI it is.
    #[must_use]
    pub fn flavor(&self) -> Flavor {
        self.flavor
    }

    /// The CLI's path.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// A command for the CLI with only the client environment.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(n, v)| (n, v)));
        command.stdin(Stdio::null());
        command
    }

    /// Runs the CLI with `args` (bounded), returning its output.
    ///
    /// # Errors
    ///
    /// `terminal.container_runtime` when it cannot be started or times
    /// out.
    pub fn run(&self, args: &[String]) -> Result<Output, Error> {
        let mut command = self.command();
        command.args(args);
        run_bounded(command, CALL_TIMEOUT).map_err(|why| {
            Error::new(
                &codes::TERMINAL_CONTAINER_RUNTIME,
                format!(
                    "{} {}: {why}",
                    self.name(),
                    args.first().map_or("", String::as_str)
                ),
            )
        })
    }

    /// Whether the runtime is reachable and rootless. A rootful daemon is
    /// accepted only under `allow_degraded = ["all"]`; Docker Desktop's
    /// daemon runs as root inside its own VM, never as host root, so it
    /// counts as rootless.
    ///
    /// # Errors
    ///
    /// `terminal.container_runtime`: unreachable, or rootful without
    /// `all`.
    pub fn probe(&self, security: &SandboxConfig) -> Result<Probe, Error> {
        let format = match self.flavor {
            Flavor::Podman => "{{.Host.Security.Rootless}}|{{.Host.OS}}",
            Flavor::Docker => "{{json .SecurityOptions}}|{{.OperatingSystem}}",
        };
        let output = self.run(&["info".into(), "--format".into(), format.into()])?;
        if !output.status.success() {
            return Err(Error::new(
                &codes::TERMINAL_CONTAINER_RUNTIME,
                format!(
                    "container runtime {} is not reachable: {}",
                    self.name(),
                    first_line(&output.stderr)
                ),
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let (security_info, os) = text.split_once('|').unwrap_or((text.as_str(), ""));
        let probe = if security_info.contains("rootless") || security_info == "true" {
            Probe {
                rootless: true,
                how: "rootless".to_owned(),
            }
        } else if os.contains("Docker Desktop") {
            Probe {
                rootless: true,
                how: "Docker Desktop VM".to_owned(),
            }
        } else {
            Probe {
                rootless: false,
                how: "rootful daemon".to_owned(),
            }
        };
        if !probe.rootless && !security.allow_degraded.contains(&DegradedFeature::All) {
            return Err(Error::new(
                &codes::TERMINAL_CONTAINER_RUNTIME,
                format!("container runtime {} runs as root; the container backend needs a rootless runtime", self.name()),
            )
            .with_hint("use rootless Podman or Docker, or set security.sandbox.allow_degraded = [\"all\"]"));
        }
        Ok(probe)
    }
}

/// `program` on `path_var`.
fn find_on_path(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    std::env::split_paths(path_var)
        .map(|dir| dir.join(format!("{name}{suffix}")))
        .find(|candidate| candidate.is_file())
}

/// The first non-empty line of `bytes`.
#[must_use]
pub fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// Runs `command` to completion, killing it after `limit`.
///
/// # Errors
///
/// It cannot start or ran past `limit`.
pub fn run_bounded(mut command: Command, limit: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let readers = [
        child
            .stdout
            .take()
            .map(|r| std::thread::spawn(move || read_all(r))),
        child
            .stderr
            .take()
            .map(|r| std::thread::spawn(move || read_all(r))),
    ];
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let [out, err] = readers.map(|r| r.and_then(|h| h.join().ok()).unwrap_or_default());
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

fn read_all(mut reader: impl std::io::Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}
