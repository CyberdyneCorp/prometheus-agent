//! The `ssh` backend's transport (spec: execution-environments, Backend
//! readiness checks; Infrastructure failures are degraded results).
//!
//! Non-secret variables travel as `env NAME=value` on the remote command
//! line. Secrets never do: they are written first to a private file in the
//! remote home through the client's stdin, which the command sources and
//! deletes before it runs.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_sandbox::ssh::{SshTarget, quote};

use super::exec::{ExecResult, run};
use super::{EnvVar, Transport};
use crate::terminal::spawn::SpawnError;

/// Where secret environment files go, relative to the remote home.
const ENV_DIR: &str = ".cache/prometheus-terminal";

/// Sources a secret environment file, deletes it, then runs the command.
/// Arguments: the file, argv.
const WITH_ENV: &str = "set -a; . \"./$1\"; set +a; rm -f -- \"./$1\"; shift; exec \"$@\"";

/// One SSH destination.
#[derive(Debug)]
pub struct SshTransport {
    target: SshTarget,
    connected: AtomicBool,
}

impl SshTransport {
    /// The transport to `target`.
    #[must_use]
    pub fn new(target: SshTarget) -> Self {
        Self {
            target,
            connected: AtomicBool::new(false),
        }
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
        let result: ExecResult = run(self, &script, &[&file], Some(body.as_bytes()))?;
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
        let result = run(self, "true", &[], None).map_err(SpawnError::Infrastructure)?;
        if result.code != 0 {
            return Err(SpawnError::Infrastructure(format!(
                "ssh {} failed: {}",
                self.target.destination(),
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        self.connected.store(true, Ordering::Release);
        Ok(())
    }

    fn evict(&self) {
        self.connected.store(false, Ordering::Release);
    }

    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String> {
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

    fn failure_code(&self) -> i32 {
        255
    }

    fn close(&self) {}
}
