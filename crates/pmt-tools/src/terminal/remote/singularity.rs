//! The `singularity` backend's transport: `apptainer exec
//! instance://<name>` into the profile's instance (spec:
//! execution-environments, Singularity backend).
//!
//! Variables reach the command as `APPTAINERENV_<NAME>` on the client
//! process only, so a secret never appears on a command line.

use std::ffi::OsString;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_sandbox::singularity::Instance;

use super::{EnvVar, Transport};
use crate::terminal::spawn::SpawnError;

/// Exec into one instance; one per session.
#[derive(Debug)]
pub struct SingularityTransport {
    instance: Arc<Instance>,
    released: AtomicBool,
}

impl SingularityTransport {
    /// The transport of `instance` (a user counted by
    /// [`Instance::shared`], released on [`Transport::close`]).
    #[must_use]
    pub fn new(instance: Arc<Instance>) -> Self {
        Self {
            instance,
            released: AtomicBool::new(false),
        }
    }
}

impl Transport for SingularityTransport {
    fn describe(&self) -> String {
        format!("singularity instance {}", self.instance.name())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        self.instance
            .ensure_running()
            .map_err(|err| SpawnError::Infrastructure(err.message().to_owned()))
    }

    fn evict(&self) {
        self.instance.evict();
    }

    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String> {
        let env: Vec<(String, OsString)> = env
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        let mut command = self.instance.exec(argv, &env);
        if stdin {
            command.stdin(std::process::Stdio::piped());
        }
        Ok(command)
    }

    fn failure_code(&self) -> i32 {
        255
    }

    fn close(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.instance.release();
        }
    }
}
