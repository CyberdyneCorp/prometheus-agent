//! The `ssh` backend's transport (spec: execution-environments, Backend
//! readiness checks; Infrastructure failures are degraded results).
//!
//! Non-secret variables travel as `env NAME=value` on the remote command
//! line. Secrets never do: they are written first to a private file in the
//! remote home through the client's stdin, which the command sources and
//! deletes before it runs.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_sandbox::ssh::{ReverseForward, SshTarget, quote};

use super::exec::{ExecResult, run};
use super::{EnvVar, Transport};
use crate::terminal::spawn::SpawnError;

/// Where secret environment files go, relative to the remote home.
const ENV_DIR: &str = ".cache/prometheus-terminal";

/// Sources a secret environment file, deletes it, then runs the command.
/// Arguments: the file, argv.
const WITH_ENV: &str = "set -a; . \"./$1\"; set +a; rm -f -- \"./$1\"; shift; exec \"$@\"";

/// Where the egress CA files go on the remote host, under its home.
const EGRESS_DIR: &str = ".cache/prometheus-terminal/egress";

/// The variables of every command given the proxy URL and the remote
/// directory holding the uploaded files: the proxy, the CA bundle and the
/// current placeholders (recomputed so a rotation reaches the next
/// command).
pub type EgressVars = Arc<dyn Fn(&str, &str) -> Vec<(String, String)> + Send + Sync>;

/// The host's egress proxy, tunnelled to the remote host (spec:
/// security-boundaries, Egress proxy coverage of containers and remote
/// sandboxes): a reverse forward from the remote loopback to `proxy`, the
/// CA files uploaded, and [`EgressVars`] on every command.
pub struct SshEgress {
    /// The proxy's loopback listener on this host.
    pub proxy: SocketAddr,
    /// File name and contents to upload (the CA bundle, the CA).
    pub files: Vec<(String, Vec<u8>)>,
    /// The variables of every command.
    pub vars: EgressVars,
}

impl std::fmt::Debug for SshEgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshEgress")
            .field("proxy", &self.proxy)
            .finish_non_exhaustive()
    }
}

/// A live tunnel: the forward and the remote directory of the files.
#[derive(Debug)]
struct Tunnel {
    forward: ReverseForward,
    dir: String,
}

/// One SSH destination.
#[derive(Debug)]
pub struct SshTransport {
    target: SshTarget,
    connected: AtomicBool,
    egress: Option<SshEgress>,
    tunnel: Mutex<Option<Tunnel>>,
}

impl SshTransport {
    /// The transport to `target`.
    #[must_use]
    pub fn new(target: SshTarget) -> Self {
        Self {
            target,
            connected: AtomicBool::new(false),
            egress: None,
            tunnel: Mutex::new(None),
        }
    }

    /// Routes every command through the host's egress proxy.
    #[must_use]
    pub fn with_egress(mut self, egress: SshEgress) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Opens the tunnel and uploads the CA files (once per connection).
    fn open_tunnel(&self, egress: &SshEgress) -> Result<(), String> {
        let mut tunnel = self.tunnel.lock().unwrap_or_else(PoisonError::into_inner);
        if tunnel.is_some() {
            return Ok(());
        }
        let forward = self
            .target
            .reverse_forward(egress.proxy)
            .map_err(|e| e.message().to_owned())?;
        let mut dir = String::new();
        for (name, contents) in &egress.files {
            let script = format!(
                "d=\"$HOME/{EGRESS_DIR}\"; umask 022; mkdir -p \"$d\" && cat > \"$d/$1\" && printf %s \"$d\""
            );
            let result = run(&Bare(self), &script, &[name], Some(contents))?;
            if result.code != 0 {
                return Err(format!(
                    "cannot upload the egress CA to {}: {}",
                    self.target.destination(),
                    String::from_utf8_lossy(&result.stderr).trim()
                ));
            }
            dir = String::from_utf8_lossy(&result.stdout).into_owned();
        }
        *tunnel = Some(Tunnel { forward, dir });
        Ok(())
    }

    /// The egress variables of a command; `None` without egress.
    fn egress_vars(&self) -> Result<Option<Vec<(String, String)>>, String> {
        let Some(egress) = &self.egress else {
            return Ok(None);
        };
        self.open_tunnel(egress)?;
        let tunnel = self.tunnel.lock().unwrap_or_else(PoisonError::into_inner);
        let tunnel = tunnel.as_ref().ok_or("the egress tunnel is down")?;
        let proxy = format!("http://127.0.0.1:{}", tunnel.forward.port());
        Ok(Some((egress.vars)(&proxy, &tunnel.dir)))
    }

    /// The client command without the egress variables.
    fn plain_command(
        &self,
        argv: &[String],
        env: &[EnvVar],
        stdin: bool,
    ) -> Result<Command, String> {
        let secrets: Vec<&EnvVar> = env.iter().filter(|v| v.secret).collect();
        let mut remote: Vec<String> = Vec::new();
        if !secrets.is_empty() {
            let file = self.upload_env(&secrets)?;
            remote.extend([
                "sh".into(),
                "-c".into(),
                WITH_ENV.into(),
                "pmt-env".into(),
                file,
            ]);
        }
        let plain: Vec<String> = env
            .iter()
            .filter(|v| !v.secret)
            .map(|v| format!("{}={}", v.name, v.value.to_string_lossy()))
            .collect();
        if !plain.is_empty() {
            remote.push("env".into());
            remote.extend(plain);
        }
        remote.extend(argv.iter().cloned());
        let mut command = self.target.command(&remote);
        if stdin {
            command.stdin(std::process::Stdio::piped());
        }
        Ok(command)
    }

    fn upload_env(&self, secrets: &[&EnvVar]) -> Result<String, String> {
        let file = format!("{ENV_DIR}/env-{}", ulid::Ulid::generate());
        let mut body = String::new();
        for var in secrets {
            body.push_str(&var.name);
            body.push('=');
            body.push_str(&quote(&var.value.to_string_lossy()));
            body.push('\n');
        }
        let script = format!("umask 077; mkdir -p {ENV_DIR} && cat > \"$1\"");
        let result: ExecResult = run(&Bare(self), &script, &[&file], Some(body.as_bytes()))?;
        if result.code == 0 {
            Ok(file)
        } else {
            Err(String::from_utf8_lossy(&result.stderr).trim().to_owned())
        }
    }
}

impl Transport for SshTransport {
    fn describe(&self) -> String {
        format!("ssh {}", self.target.destination())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = run(&Bare(self), "true", &[], None).map_err(SpawnError::Infrastructure)?;
        if result.code != 0 {
            return Err(SpawnError::Infrastructure(format!(
                "ssh {} failed: {}",
                self.target.destination(),
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        if let Some(egress) = &self.egress {
            // Fail closed: no command runs outside the proxy.
            self.open_tunnel(egress)
                .map_err(SpawnError::Infrastructure)?;
        }
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    fn evict(&self) {
        self.connected.store(false, Ordering::Release);
        self.tunnel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String> {
        let Some(overrides) = self.egress_vars()? else {
            return self.plain_command(argv, env, stdin);
        };
        let overridden = |name: &str| overrides.iter().any(|(n, _)| n == name);
        let mut env: Vec<EnvVar> = env
            .iter()
            .filter(|v| !overridden(&v.name))
            .cloned()
            .collect();
        env.extend(overrides.iter().map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.into(),
            secret: false,
        }));
        self.plain_command(argv, &env, stdin)
    }

    fn failure_code(&self) -> i32 {
        255
    }

    fn close(&self) {
        self.tunnel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// The transport without the egress variables: the tunnel's own setup
/// calls and the secret file uploads.
#[derive(Debug)]
struct Bare<'a>(&'a SshTransport);

impl Transport for Bare<'_> {
    fn describe(&self) -> String {
        self.0.describe()
    }

    fn connect(&self) -> Result<(), SpawnError> {
        Ok(())
    }

    fn evict(&self) {}

    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String> {
        self.0.plain_command(argv, env, stdin)
    }

    fn failure_code(&self) -> i32 {
        self.0.failure_code()
    }

    fn close(&self) {}
}
