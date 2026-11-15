//! Starting workers (spec: kanban, Worker spawn by run ticket): the
//! dispatcher runs `prometheus kanban work --run <run-id>` with a
//! scrubbed environment (only `PATH`, the locale variables and
//! `PROMETHEUS_HOME`) and the run ticket as one line on stdin; output goes
//! to `<board>/logs/<task>-<run>.log`. The worker resolves everything
//! else from the ticket, under its assignee profile.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_kanban::dispatch::{SpawnRequest, Spawner};

/// Spawns workers as child processes of this binary, and worker lanes
/// as their brokers (which run the plugins' sandboxed commands).
pub struct ProcessSpawner {
    exe: PathBuf,
    home: PathBuf,
    env: Vec<(String, OsString)>,
    /// The dispatching profile.
    profile: String,
}

/// Whether the launch variable `name` reaches a worker: `PATH` and the
/// locale (`LANG`, `LANGUAGE`, `LC_*`), plus on Windows the system
/// directory variables without which no process starts there.
#[must_use]
pub fn passes(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let windows_system = cfg!(windows) && matches!(upper.as_str(), "SYSTEMROOT" | "WINDIR");
    upper == "PATH"
        || upper == "LANG"
        || upper == "LANGUAGE"
        || upper.starts_with("LC_")
        || windows_system
}

/// The environment of a worker of the home at `home`.
#[must_use]
pub fn worker_env(launch: &LaunchEnv, home: &Path) -> Vec<(String, OsString)> {
    let mut env: Vec<(String, OsString)> = launch
        .inherited_vars()
        .filter(|(name, _)| passes(name))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    env.push(("PROMETHEUS_HOME".to_owned(), home.as_os_str().to_owned()));
    env
}

impl ProcessSpawner {
    /// A spawner of this executable's workers for the home of `scope` (the
    /// dispatching profile, whose plugins declare the lanes).
    ///
    /// # Errors
    ///
    /// `home.io` when the running executable cannot be found.
    pub fn new(scope: &ProfileScope, launch: &LaunchEnv) -> Result<Self, Error> {
        let home = scope.home();
        let exe = std::env::current_exe().map_err(|e| {
            Error::new(
                &codes::HOME_IO,
                format!("cannot locate this executable: {e}"),
            )
        })?;
        Ok(Self {
            exe,
            home: home.to_path_buf(),
            env: worker_env(launch, home),
            profile: scope.name().to_string(),
        })
    }

    fn log(&self, request: &SpawnRequest<'_>) -> Result<std::fs::File, String> {
        let dir = self
            .home
            .join("kanban/boards")
            .join(request.board)
            .join("logs");
        let path = dir.join(format!("{}-{}.log", request.task.id, request.run_id));
        pmt_core::private_fs::append_options()
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))
    }
}

impl Spawner for ProcessSpawner {
    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<(u32, u64), String> {
        let log = self.log(request)?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut command = Command::new(&self.exe);
        // A lane task runs under its broker (outside the lane's sandbox),
        // in the dispatcher's profile, whose plugins declare the lanes.
        if request.lane {
            command.args([
                "-p",
                self.profile.as_str(),
                "kanban",
                "lane",
                "--run",
                request.run_id,
            ]);
        } else {
            command.args(["kanban", "work", "--run", request.run_id]);
        }
        command
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(log)
            .stderr(err);
        // A worker outlives a stopped dispatcher's terminal signals.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start the worker: {e}"))?;
        let pid = child.id();
        if let Some(mut stdin) = child.stdin.take() {
            let line = format!("{}\n", request.ticket.line());
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| format!("cannot hand the worker its ticket: {e}"))?;
        }
        let start = start_time(pid);
        // Reap the child when it exits, so it never lingers as a zombie;
        // the board learns its exit from the run row.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        start
            .ok_or_else(|| "the worker exited before it could be identified".to_owned())
            .map(|start| (pid, start))
    }
}

/// The start time of a just-spawned process (the OS may list it a
/// moment late).
pub(super) fn start_time(pid: u32) -> Option<u64> {
    for _ in 0..50 {
        if let Some(start) = pmt_core::runtime::process_start_time(pid) {
            return Some(start);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_path_locale_and_the_home_reach_a_worker() {
        let launch = LaunchEnv::from_vars(
            [
                ("PATH", "/usr/bin"),
                ("LANG", "pt_BR.UTF-8"),
                ("LC_TIME", "C"),
                ("HOME", "/home/ops"),
                ("OPENAI_API_KEY", "sk-ops-secret"),
                ("AWS_PROFILE", "ops"),
            ]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
            &pmt_core::env::EnvPolicy {
                provider_secret_names: &["OPENAI_API_KEY"],
                replacements: &[],
            },
        );
        let env = worker_env(&launch, Path::new("/srv/pmt"));
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"PATH") && names.contains(&"LANG") && names.contains(&"LC_TIME"));
        assert!(names.contains(&"PROMETHEUS_HOME"));
        assert!(
            !names
                .iter()
                .any(|n| matches!(*n, "HOME" | "OPENAI_API_KEY" | "AWS_PROFILE"))
        );
    }
}
