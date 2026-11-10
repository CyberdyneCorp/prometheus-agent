//! Supervising the managed server (spec: local-models, Managed server
//! lifecycle): start it, wait until it answers `/health`, record it, and
//! restart it when it crashes (1 s doubling to 60 s, at most 5 attempts in
//! a row). Supervision ends, stopping the server, when
//!
//! - the [`Stop`] flag is raised (the supervising process is exiting: its
//!   client closed the helper's stdin, or Ctrl+C in `local start`), or
//! - the record was removed (`prometheus local stop`), which also tells a
//!   crash (record still there) from a requested stop.
//!
//! This is blocking code on its own thread: it outlives no process and
//! needs no async runtime.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};

use super::launch::Launch;
use super::record::{self, ServerRecord};
use crate::download::{Owner, io_error};

/// Restarts tried in a row before supervision gives up.
pub const MAX_RESTARTS: u32 = 5;
/// The first restart delay.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// The longest restart delay.
pub const MAX_BACKOFF: Duration = Duration::from_mins(1);
/// How long a start may take before it counts as failed (model files are
/// loaded lazily, so this is the engine's own start).
pub const START_TIMEOUT: Duration = Duration::from_mins(2);
/// A server that ran this long resets the restart count.
const STABLE: Duration = Duration::from_mins(1);
const POLL: Duration = Duration::from_millis(100);

/// Raised to stop supervision.
#[derive(Debug, Clone, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    /// Stops supervision (and the server) soon.
    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether it was raised.
    #[must_use]
    pub fn raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Supervises `launch` for `home` until stopped; `ready` is called with
/// the record each time the server answers.
///
/// # Errors
///
/// `provider.local_runtime_down` (69) once the server failed
/// [`MAX_RESTARTS`] times in a row; `runtime.io`.
pub fn supervise(
    home: &Path,
    launch: &Launch,
    stop: &Stop,
    ready: &mut dyn FnMut(&ServerRecord),
) -> Result<(), Error> {
    let mut failures = 0;
    loop {
        let began = Instant::now();
        let outcome = run_once(home, launch, stop, ready);
        match outcome {
            Ok(Ended::Stopped) => return Ok(()),
            Ok(Ended::Crashed(why)) | Err(Crash(why)) => {
                if began.elapsed() >= STABLE {
                    failures = 0;
                }
                failures += 1;
                tracing::warn!(
                    component = "local",
                    attempt = failures,
                    "llama-server: {why}"
                );
                if failures > MAX_RESTARTS {
                    record::remove(home);
                    return Err(Error::new(
                        &codes::PROVIDER_LOCAL_RUNTIME_DOWN,
                        format!(
                            "the local model server isn't running: it failed {MAX_RESTARTS} restarts ({why})"
                        ),
                    )
                    .with_hint(format!("see {}", launch.log.display())));
                }
                if wait(backoff(failures), stop) {
                    record::remove(home);
                    return Ok(());
                }
            }
        }
    }
}

/// The delay before restart number `n` (1-based).
#[must_use]
pub fn backoff(n: u32) -> Duration {
    FIRST_BACKOFF
        .saturating_mul(1 << n.saturating_sub(1).min(16))
        .min(MAX_BACKOFF)
}

enum Ended {
    Stopped,
    Crashed(String),
}

struct Crash(String);

impl From<Error> for Crash {
    fn from(err: Error) -> Self {
        Self(err.message().to_owned())
    }
}

fn run_once(
    home: &Path,
    launch: &Launch,
    stop: &Stop,
    ready: &mut dyn FnMut(&ServerRecord),
) -> Result<Ended, Crash> {
    let mut child = spawn(launch)?;
    let guard = KillOnDrop(&mut child);
    if !healthy(guard.0, launch.port, stop) {
        return Ok(if stop.raised() {
            Ended::Stopped
        } else {
            Ended::Crashed("it did not answer /health".to_owned())
        });
    }
    let record = ServerRecord {
        pid: guard.0.id(),
        start_time: pmt_core::runtime::process_start_time(guard.0.id()).unwrap_or(0),
        port: launch.port,
        supervisor: owner(),
        engine: launch.engine.clone(),
        backend: launch.backend,
        started_at: jiff::Timestamp::now().to_string(),
    };
    record::write(home, &record)?;
    ready(&record);
    loop {
        if stop.raised() {
            record::remove(home);
            return Ok(Ended::Stopped);
        }
        if let Ok(Some(status)) = guard.0.try_wait() {
            // `local stop` removes the record before ending the server.
            return Ok(if record::read(home).is_some_and(|r| r.pid == record.pid) {
                record::remove(home);
                Ended::Crashed(format!("it exited ({status})"))
            } else {
                Ended::Stopped
            });
        }
        if record::read(home).is_none_or(|r| r.pid != record.pid) {
            return Ok(Ended::Stopped);
        }
        std::thread::sleep(POLL * 2);
    }
}

/// Kills the server when supervision of it ends, however it ends.
struct KillOnDrop<'a>(&'a mut Child);

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn spawn(launch: &Launch) -> Result<Child, Error> {
    if let Some(dir) = launch.log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    }
    let mut log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&launch.log)
        .map_err(|e| io_error(&launch.log, &e))?;
    let _ = writeln!(
        log,
        "--- {} starting {} on port {}",
        jiff::Timestamp::now(),
        launch.exe.display(),
        launch.port
    );
    let err = log.try_clone().map_err(|e| io_error(&launch.log, &e))?;
    launch
        .command()
        .stdout(log)
        .stderr(err)
        .spawn()
        .map_err(|e| {
            Error::new(
                &codes::PROVIDER_LOCAL_RUNTIME_DOWN,
                format!(
                    "the local model server isn't running: cannot start {}: {e}",
                    launch.exe.display()
                ),
            )
        })
}

/// Waits until `/health` answers 200; false when the child exits, the
/// start times out or a stop is raised.
fn healthy(child: &mut Child, port: u16, stop: &Stop) -> bool {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline && !stop.raised() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }
        if health(port) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// A blocking `GET /health` on the loopback port.
#[must_use]
pub fn health(port: u16) -> bool {
    use std::io::Read as _;
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0; 12];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.1 200")
}

/// Sleeps `delay` unless a stop is raised first (true: stopped).
fn wait(delay: Duration, stop: &Stop) -> bool {
    let until = Instant::now() + delay;
    while Instant::now() < until {
        if stop.raised() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    stop.raised()
}

fn owner() -> Owner {
    let pid = std::process::id();
    Owner {
        pid,
        start_time: pmt_core::runtime::process_start_time(pid).unwrap_or(0),
    }
}

/// Stops the managed server of `home`: removes its record (so its
/// supervisor does not restart it) and ends the process. Returns the
/// stopped record.
#[must_use]
pub fn stop_server(home: &Path) -> Option<ServerRecord> {
    let record = record::live(home)?;
    record::remove(home);
    kill(record.pid);
    Some(record)
}

fn kill(pid: u32) {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    if let Some(process) = system.process(pid) {
        let _ = process.kill();
    }
}

/// The log of `home`'s server.
#[must_use]
pub fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join(super::launch::LOG_FILE)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::backoff;

    #[test]
    fn restarts_back_off_from_one_second_doubling_to_a_minute() {
        let delays: Vec<Duration> = (1..=8).map(backoff).collect();
        assert_eq!(delays[..5], [1, 2, 4, 8, 16].map(Duration::from_secs));
        assert_eq!(delays[7], Duration::from_mins(1));
    }
}
