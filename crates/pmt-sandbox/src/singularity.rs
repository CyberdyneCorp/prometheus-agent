//! The built-in `singularity` backend's runtime side (spec:
//! execution-environments, Singularity backend).
//!
//! Commands run in one Apptainer (or `SingularityCE`) instance per profile,
//! started from `terminal.singularity.image` (a SIF path or a `docker://`
//! reference) with `--containall` and the profile's persistent overlay
//! directory, then reached with `exec instance://<name>`. Every call runs
//! the CLI with the [`crate::container::CLIENT_ENV`] allowlist; variables
//! for a command travel as `APPTAINERENV_<NAME>` (`SINGULARITYENV_` for
//! `SingularityCE`) on the client process only, with `--cleanenv`, so a
//! secret never appears on a command line and nothing else of the host
//! environment reaches the command.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use sha2::{Digest, Sha256};

use crate::container::{client_env, first_line, run_bounded};

/// How long `instance start` may take (a `docker://` image is converted on
/// first use).
const START_TIMEOUT: Duration = Duration::from_mins(10);
/// How long `instance stop` may take.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// The Apptainer or `SingularityCE` CLI.
#[derive(Debug, Clone)]
pub struct Runtime {
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Runtime {
    /// `apptainer` on `PATH`, else `singularity`.
    ///
    /// # Errors
    ///
    /// `terminal.backend_unavailable` when neither is installed.
    pub fn detect() -> Result<Self, Error> {
        let env = client_env();
        let path = env
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let program = ["apptainer", "singularity"]
            .iter()
            .find_map(|name| {
                std::env::split_paths(&path)
                    .map(|d| d.join(name))
                    .find(|p| p.is_file())
            })
            .ok_or_else(|| {
                Error::new(
                    &codes::TERMINAL_BACKEND_UNAVAILABLE,
                    "singularity backend requires apptainer or singularity on PATH",
                )
                .with_hint("install Apptainer (https://apptainer.org) or SingularityCE")
            })?;
        Ok(Self { program, env })
    }

    /// The CLI at `program` with client environment `env`.
    #[must_use]
    pub fn at(program: PathBuf, env: Vec<(OsString, OsString)>) -> Self {
        Self { program, env }
    }

    /// `apptainer` or `singularity`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        if self.is_singularity() {
            "singularity"
        } else {
            "apptainer"
        }
    }

    fn is_singularity(&self) -> bool {
        self.program
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("singularity"))
    }

    /// The prefix that hands a variable to the command
    /// (`APPTAINERENV_` / `SINGULARITYENV_`).
    #[must_use]
    pub fn env_prefix(&self) -> &'static str {
        if self.is_singularity() {
            "SINGULARITYENV_"
        } else {
            "APPTAINERENV_"
        }
    }

    /// A command for the CLI with only the client environment, started in
    /// `/` (the CLI otherwise tries to enter the host's directory inside).
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(n, v)| (n, v)))
            .current_dir("/")
            .stdin(Stdio::null());
        command
    }
}

/// What a profile's instance is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// The profile it belongs to.
    pub profile: String,
    /// `terminal.singularity.image`.
    pub image: String,
    /// The profile's persistent overlay directory.
    pub overlay: PathBuf,
}

impl InstanceSpec {
    /// The instance name: the profile and a digest of the image and the
    /// overlay, so another setting gets another instance.
    #[must_use]
    pub fn name(&self) -> String {
        let profile: String = self
            .profile
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(32)
            .collect();
        let digest = Sha256::digest(format!("{}\0{}", self.image, self.overlay.display()));
        let hex = digest[..5].iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        });
        format!("pmt-{profile}-{hex}")
    }

    /// The `instance start` arguments.
    #[must_use]
    pub fn start_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = ["instance", "start", "--containall", "--overlay"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.push(self.overlay.clone().into_os_string());
        args.push(self.image.clone().into());
        args.push(self.name().into());
        args
    }
}

/// One profile's instance, shared by its sessions in this process.
#[derive(Debug)]
pub struct Instance {
    runtime: Runtime,
    spec: InstanceSpec,
    name: String,
    /// Whether this process knows it to be running, and whether it started
    /// it (only then does the last session stop it).
    state: Mutex<State>,
    users: AtomicUsize,
}

#[derive(Debug, Default)]
struct State {
    running: bool,
    started_here: bool,
}

static INSTANCES: LazyLock<Mutex<HashMap<String, Weak<Instance>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl Instance {
    /// The instance of `spec`, shared with every session of this process
    /// already using it; counts one more user (see [`Self::release`]).
    #[must_use]
    pub fn shared(runtime: Runtime, spec: InstanceSpec) -> Arc<Self> {
        let name = spec.name();
        let mut map = INSTANCES.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, i| i.strong_count() > 0);
        let instance = map.get(&name).and_then(Weak::upgrade).unwrap_or_else(|| {
            let instance = Arc::new(Self {
                runtime,
                spec,
                name: name.clone(),
                state: Mutex::new(State::default()),
                users: AtomicUsize::new(0),
            });
            map.insert(name, Arc::downgrade(&instance));
            instance
        });
        instance.users.fetch_add(1, Ordering::AcqRel);
        instance
    }

    /// Its name (`instance://<name>`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The CLI it runs on.
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Makes sure the instance runs, starting it (and creating the overlay
    /// directory) when needed; an instance of the same name another
    /// process started is used as it is. Blocks.
    ///
    /// # Errors
    ///
    /// `terminal.backend_unavailable` naming the CLI's first error line.
    pub fn ensure_running(&self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.running {
            return Ok(());
        }
        pmt_core::private_fs::create_dir(&self.spec.overlay).map_err(|e| {
            unavailable(format!(
                "cannot create the overlay {}: {e}",
                self.spec.overlay.display()
            ))
        })?;
        let mut command = self.runtime.command();
        command.args(self.spec.start_args());
        let output = run_bounded(command, START_TIMEOUT)
            .map_err(|why| unavailable(format!("{} instance start: {why}", self.runtime.name())))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            state.started_here = true;
        } else if !stderr.contains("already exists") {
            return Err(unavailable(format!(
                "{} instance start failed: {}",
                self.runtime.name(),
                first_line(&output.stderr)
            )));
        }
        state.running = true;
        Ok(())
    }

    /// The client command running `argv` in the instance with `env` set
    /// on the client process only (`--cleanenv` keeps the rest out).
    #[must_use]
    pub fn exec<S: AsRef<std::ffi::OsStr>>(
        &self,
        argv: &[S],
        env: &[(String, OsString)],
    ) -> Command {
        let mut command = self.runtime.command();
        for (name, value) in env {
            command.env(format!("{}{name}", self.runtime.env_prefix()), value);
        }
        command
            .args(["exec", "--cleanenv"])
            .arg(format!("instance://{}", self.name))
            .args(argv);
        command
    }

    /// Forgets that the instance runs (after a failure), so the next call
    /// starts it again.
    pub fn evict(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
    }

    /// One session stopped using the instance; the last one stops it when
    /// this process started it. The overlay stays. Blocks.
    pub fn release(&self) {
        if self.users.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.started_here {
            return;
        }
        let mut command = self.runtime.command();
        command.args(["instance", "stop", &self.name]);
        if let Err(why) = run_bounded(command, STOP_TIMEOUT) {
            tracing::warn!(instance = %self.name, "instance stop failed: {why}");
        }
        *state = State::default();
    }
}

/// The profile's overlay directory under its home.
#[must_use]
pub fn overlay_dir(profile_home: &Path) -> PathBuf {
    profile_home.join("terminal").join("singularity-overlay")
}

fn unavailable(message: String) -> Error {
    Error::new(&codes::TERMINAL_BACKEND_UNAVAILABLE, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(image: &str) -> InstanceSpec {
        InstanceSpec {
            profile: "work/1".to_owned(),
            image: image.to_owned(),
            overlay: PathBuf::from("/p/terminal/singularity-overlay"),
        }
    }

    #[test]
    fn the_instance_starts_contained_on_the_profile_overlay() {
        let spec = spec("docker://alpine:3.20");
        let args: Vec<String> = spec
            .start_args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[..5],
            [
                "instance",
                "start",
                "--containall",
                "--overlay",
                "/p/terminal/singularity-overlay"
            ]
        );
        assert_eq!(args[5], "docker://alpine:3.20");
        assert_eq!(args[6], spec.name());
        assert!(spec.name().starts_with("pmt-work-1-"), "{}", spec.name());
        assert_ne!(spec.name(), self::spec("/images/other.sif").name());
    }
}
